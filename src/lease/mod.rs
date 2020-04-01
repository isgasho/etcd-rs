//! Leases are a mechanism for detecting client liveness. The cluster grants leases with a time-to-live. A lease expires if the etcd cluster does not receive a keepAlive within a given TTL period.
//!
//! # Examples
//!
//! Grant lease and keep lease alive
//!
//! ```no_run
//! use std::time::Duration;
//!
//! use etcd_rs::*;
//!
//! #[tokio::main]
//! async fn main() -> Result<()> {
//!     let client = Client::connect(ClientConfig {
//!         endpoints: vec!["http://127.0.0.1:2379".to_owned()],
//!         auth: None,
//!     }).await?;
//!
//!     let key = "foo";
//!
//!     // grant lease
//!     let lease = client
//!         .lease()
//!         .grant(LeaseGrantRequest::new(Duration::from_secs(3)))
//!         .await?;
//!
//!     let lease_id = lease.id();
//!
//!     // set key with lease
//!     client
//!         .kv()
//!         .put({
//!             let mut req = PutRequest::new(key, "bar");
//!             req.set_lease(lease_id);
//!
//!             req
//!         })
//!         .await?;
//!
//!     {
//!         // keep alive the lease every 1 second
//!         let client = client.clone();
//!
//!         let mut interval = tokio::time::interval(Duration::from_secs(1));
//!
//!         loop {
//!             interval.tick().await;
//!             client
//!                 .lease()
//!                 .keep_alive(LeaseKeepAliveRequest::new(lease_id))
//!                 .await;
//!         }
//!     }
//!
//!     Ok(())
//! }
//! ```

mod grant;
mod keep_alive;
mod revoke;
pub use grant::{LeaseGrantRequest, LeaseGrantResponse};
pub use keep_alive::{LeaseKeepAliveRequest, LeaseKeepAliveResponse};
pub use revoke::{LeaseRevokeRequest, LeaseRevokeResponse};

use std::sync::Arc;

use tokio::stream::Stream;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tonic::transport::Channel;

use crate::lazy::Lazy;
use crate::proto::etcdserverpb;
use crate::proto::etcdserverpb::lease_client::LeaseClient;
use crate::Result as Res;

/// LeaseKeepAliveTunnel is a reusable connection for `Lease Keep Alive` operation.
/// The underlying gRPC method is Bi-directional streaming.
struct LeaseKeepAliveTunnel {
    req_sender: UnboundedSender<LeaseKeepAliveRequest>,
    resp_receiver: Option<UnboundedReceiver<Result<LeaseKeepAliveResponse, tonic::Status>>>,
}

impl LeaseKeepAliveTunnel {
    fn new(mut client: LeaseClient<Channel>) -> Self {
        let (req_sender, mut req_receiver) = unbounded_channel::<LeaseKeepAliveRequest>();
        let (resp_sender, resp_receiver) =
            unbounded_channel::<Result<LeaseKeepAliveResponse, tonic::Status>>();

        let request = tonic::Request::new(async_stream::stream! {
            while let Some(req) = req_receiver.recv().await {
                let pb: etcdserverpb::LeaseKeepAliveRequest = req.into();
                yield pb;
            }
        });

        // monitor inbound watch response and transfer to the receiver
        tokio::spawn(async move {
            let mut inbound = client.lease_keep_alive(request).await.unwrap().into_inner();

            loop {
                let resp = inbound.message().await;
                match resp {
                    Ok(Some(resp)) => {
                        resp_sender.send(Ok(From::from(resp))).unwrap();
                    }
                    Ok(None) => {
                        return;
                    }
                    Err(e) => {
                        resp_sender.send(Err(e)).unwrap();
                    }
                };
            }
        });

        Self {
            req_sender,
            resp_receiver: Some(resp_receiver),
        }
    }

    fn take_resp_receiver(
        &mut self,
    ) -> UnboundedReceiver<Result<LeaseKeepAliveResponse, tonic::Status>> {
        self.resp_receiver
            .take()
            .expect("take the unique watch response receiver")
    }
}

/// Lease client.
#[derive(Clone)]
pub struct Lease {
    client: LeaseClient<Channel>,
    keep_alive_tunnel: Arc<Lazy<LeaseKeepAliveTunnel>>,
}

impl Lease {
    pub(crate) fn new(client: LeaseClient<Channel>) -> Self {
        let keep_alive_tunnel = {
            let client = client.clone();
            Arc::new(Lazy::new(move || LeaseKeepAliveTunnel::new(client.clone())))
        };
        Self {
            client,
            keep_alive_tunnel,
        }
    }

    /// Performs a lease granting operation.
    pub async fn grant(&mut self, req: LeaseGrantRequest) -> Res<LeaseGrantResponse> {
        let resp = self
            .client
            .lease_grant(tonic::Request::new(req.into()))
            .await?;

        Ok(From::from(resp.into_inner()))
    }

    /// Performs a lease revoking operation.
    pub async fn revoke(&mut self, req: LeaseRevokeRequest) -> Res<LeaseRevokeResponse> {
        let resp = self
            .client
            .lease_revoke(tonic::Request::new(req.into()))
            .await?;

        Ok(From::from(resp.into_inner()))
    }

    /// Fetch keep alive response stream.
    pub async fn keep_alive_responses(
        &mut self,
    ) -> impl Stream<Item = Result<LeaseKeepAliveResponse, tonic::Status>> {
        self.keep_alive_tunnel.write().await.take_resp_receiver()
    }

    /// Performs a lease refreshing operation.
    pub async fn keep_alive(&mut self, req: LeaseKeepAliveRequest) {
        self.keep_alive_tunnel
            .write()
            .await
            .req_sender
            .send(req)
            .unwrap();
    }
}
