mod health;
mod metadata;
mod stream;

use std::{net::SocketAddr, sync::Arc};

use apibara_core::node as node_pb;
use apibara_node::db::libmdbx::{Environment, EnvironmentKind};
use tokio::task::JoinError;
use tokio_util::sync::CancellationToken;
use tonic::transport::Server as TonicServer;
use tracing::{error, info, info_span};

use crate::{
    db::DatabaseStorage, healer::HealerClient, ingestion::IngestionStreamClient,
    server::stream::StreamService,
};

use self::health::HealthReporter;

pub use self::metadata::{
    MetadataKeyRequestObserver, RequestMeter, RequestObserver, SimpleRequestObserver,
};

pub struct Server<E: EnvironmentKind, O: RequestObserver> {
    db: Arc<Environment<E>>,
    ingestion: Arc<IngestionStreamClient>,
    healer: Arc<HealerClient>,
    request_observer: O,
}

#[derive(thiserror::Error, Debug)]
pub enum ServerError {
    #[error("grpc transport error")]
    Transport(#[from] tonic::transport::Error),
    #[error("error awaiting task")]
    Task(#[from] JoinError),
    #[error("error starting reflection server")]
    ReflectionServer(#[from] tonic_reflection::server::Error),
}

impl<E, O> Server<E, O>
where
    E: EnvironmentKind,
    O: RequestObserver,
{
    pub fn new(
        db: Arc<Environment<E>>,
        ingestion: IngestionStreamClient,
        healer: HealerClient,
    ) -> Server<E, SimpleRequestObserver> {
        let ingestion = Arc::new(ingestion);
        let healer = Arc::new(healer);
        let request_observer = SimpleRequestObserver::default();
        Server {
            db,
            ingestion,
            healer,
            request_observer,
        }
    }

    /// Creates a new Server with the given request observer.
    pub fn with_request_observer<S: RequestObserver>(self, request_observer: S) -> Server<E, S> {
        Server {
            db: self.db,
            ingestion: self.ingestion,
            healer: self.healer,
            request_observer,
        }
    }

    pub async fn start(self, addr: SocketAddr, ct: CancellationToken) -> Result<(), ServerError> {
        let (mut health_reporter, health_service) = HealthReporter::new(self.db.clone());

        let reporter_handle = tokio::spawn({
            let ct = ct.clone();
            async move { health_reporter.start(ct).await }
        });

        let reflection_service = tonic_reflection::server::Builder::configure()
            .register_encoded_file_descriptor_set(node_pb::v1alpha2::node_file_descriptor_set())
            .build()?;

        let storage = DatabaseStorage::new(self.db);
        let stream_service =
            StreamService::new(self.ingestion, self.healer, storage, self.request_observer)
                .into_service();

        info!(addr = %addr, "starting server");

        TonicServer::builder()
            .trace_fn(|_| info_span!("node_server"))
            .add_service(health_service)
            .add_service(stream_service)
            .add_service(reflection_service)
            .serve_with_shutdown(addr, {
                let ct = ct.clone();
                async move { ct.cancelled().await }
            })
            .await?;

        // signal health reporter to stop and wait for it
        ct.cancel();
        reporter_handle.await?;

        Ok(())
    }
}
