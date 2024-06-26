use anyhow::{anyhow, bail};
use arroyo_compiler_service;
use arroyo_server_common::shutdown::Shutdown;
use arroyo_server_common::{log_event, start_admin_server};
use arroyo_types::{ports, DatabaseConfig};
use arroyo_worker::WorkerServer;
use clap::{Parser, Subcommand};
use deadpool_postgres::{ManagerConfig, Pool, RecyclingMethod};
use serde_json::json;
use std::process::exit;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::time::timeout;
use tokio_postgres::{Client, Connection, NoTls};
use tracing::{debug, error, info};
use uuid::Uuid;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Starts an Arroyo API server
    Api {},

    /// Starts an Arroyo Controller
    Controller {},

    /// Starts a complete Arroyo cluster
    Cluster {},

    /// Starts an Arroyo worker
    Worker {},

    /// Starts an Arroyo compiler
    Compiler {},

    /// Starts an Arroyo node server
    Node {},

    /// Runs database migrations on the configure Postgres database
    Migrate {
        /// If set, waits for the specified number of seconds until Postgres is ready before running migrations
        #[arg(long)]
        wait: Option<u32>,
    },
}

#[derive(Debug, Eq, PartialEq, Copy, Clone)]
enum CPService {
    Api,
    Compiler,
    Controller,
    All,
}

impl CPService {
    pub fn name(&self) -> &'static str {
        match self {
            CPService::Api => "api",
            CPService::Compiler => "compiler",
            CPService::Controller => "controller",
            CPService::All => "cluster",
        }
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    match &cli.command {
        Commands::Api { .. } => {
            start_control_plane(CPService::Api).await;
        }
        Commands::Compiler { .. } => {
            start_control_plane(CPService::Compiler).await;
        }
        Commands::Controller { .. } => {
            start_control_plane(CPService::Controller).await;
        }
        Commands::Cluster { .. } => {
            start_control_plane(CPService::All).await;
        }
        Commands::Worker { .. } => {
            start_worker().await;
        }
        Commands::Migrate { wait } => {
            if let Err(e) = migrate(*wait).await {
                error!("{}", e);
                exit(1);
            }
        }
        Commands::Node { .. } => {
            start_node().await;
        }
    };
}

async fn db_pool() -> Pool {
    let config = DatabaseConfig::load();
    let mut cfg = deadpool_postgres::Config::new();
    cfg.dbname = Some(config.name.clone());
    cfg.host = Some(config.host.clone());
    cfg.port = Some(config.port);
    cfg.user = Some(config.user.clone());
    cfg.password = Some(config.password.clone());
    cfg.manager = Some(ManagerConfig {
        recycling_method: RecyclingMethod::Fast,
    });
    let pool = cfg
        .create_pool(Some(deadpool_postgres::Runtime::Tokio1), NoTls)
        .unwrap_or_else(|e| {
            error!("Unable to connect to database {}: {:?}", config, e);
            exit(1);
        });

    match pool
        .get()
        .await
        .unwrap_or_else(|e| {
            error!("Unable to create database connection for {} {}", config, e);
            exit(1);
        })
        .query_one("select id from cluster_info", &[])
        .await
    {
        Ok(row) => {
            let uuid: Uuid = row.get(0);
            arroyo_server_common::set_cluster_id(&uuid.to_string());
        }
        Err(e) => {
            panic!("Failed to get cluster info {:?}", e);
        }
    };

    pool
}

mod migrations {
    use refinery::embed_migrations;
    embed_migrations!("../arroyo-api/migrations");
}

async fn connect(
    retry: bool,
) -> anyhow::Result<(
    Client,
    Connection<impl AsyncRead + AsyncWrite + Unpin, impl AsyncRead + AsyncWrite + Unpin>,
)> {
    let config = DatabaseConfig::load();

    loop {
        match tokio_postgres::config::Config::new()
            .host(&config.host)
            .port(config.port)
            .user(&config.user)
            .password(&config.password)
            .dbname(&config.name)
            .connect(NoTls)
            .await
        {
            Ok(r) => {
                return Ok(r);
            }
            Err(e) => {
                if !e.to_string().contains("authentication") && retry {
                    debug!("Received error from database while waiting: {}", e);
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    continue;
                }

                bail!("Failed to connect to database {}: {}", config, e);
            }
        }
    }
}

async fn migrate(wait: Option<u32>) -> anyhow::Result<()> {
    let _guard = arroyo_server_common::init_logging("migrate");

    let (mut client, connection) = if let Some(wait) = wait {
        info!("Waiting for database to be ready to run migrations");
        timeout(Duration::from_secs(wait as u64), connect(true))
            .await
            .map_err(|e| anyhow!("Timed out waiting for database to connect after {}", e))??
    } else {
        connect(false).await?
    };

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("connection error: {}", e);
        }
    });

    info!("Running migrations on database {}", DatabaseConfig::load());

    let report = migrations::migrations::runner()
        .run_async(&mut client)
        .await
        .map_err(|e| {
            anyhow!(
                "Failed to run migrations on {}: {:?}",
                DatabaseConfig::load(),
                e
            )
        })?;

    for migration in report.applied_migrations() {
        info!("Applying V{} {}", migration.version(), migration.name());
    }

    info!(
        "Successfully applied {} migration(s)",
        report.applied_migrations().len()
    );

    Ok(())
}

async fn start_control_plane(service: CPService) {
    let _guard = arroyo_server_common::init_logging(service.name());

    let pool = db_pool().await;

    log_event(
        "service_startup",
        json!({
            "service": service.name(),
            "scheduler": std::env::var("SCHEDULER").unwrap_or_else(|_| "process".to_string())
        }),
    );

    let shutdown = Shutdown::new(service.name());

    shutdown.spawn_task(
        "admin",
        start_admin_server(service.name(), ports::API_ADMIN),
    );

    if service == CPService::Api || service == CPService::All {
        shutdown.spawn_task("api", arroyo_api::start_server(pool.clone()));
    }

    if service == CPService::Compiler || service == CPService::All {
        shutdown.spawn_task("compiler", arroyo_compiler_service::start_service());
    }

    if service == CPService::Controller || service == CPService::All {
        arroyo_controller::ControllerServer::new(pool)
            .await
            .start(shutdown.guard("controller"));
    }

    let _ = shutdown.wait_for_shutdown(Duration::from_secs(30)).await;
}

async fn start_worker() {
    let shutdown = Shutdown::new("worker");
    let server = WorkerServer::from_env(shutdown.guard("worker"));

    let _guard = arroyo_server_common::init_logging(&format!(
        "worker-{}-{}",
        server.id().0,
        server.job_id()
    ));

    shutdown.spawn_task("admin", start_admin_server("worker", 0));
    let token = shutdown.token();
    tokio::spawn(async move {
        if let Err(e) = server.start_async().await {
            error!("Failed to start worker server: {:?}", e);
            token.cancel();
        }
    });

    let _ = shutdown.wait_for_shutdown(Duration::from_secs(30)).await;
}

async fn start_node() {
    let shutdown = Shutdown::new("node");
    let id = arroyo_node::start_server(shutdown.guard("node")).await;

    let _guard = arroyo_server_common::init_logging(&format!("node-{}", id.0,));

    shutdown.spawn_task("admin", start_admin_server("worker", 0));

    let _ = shutdown.wait_for_shutdown(Duration::from_secs(30)).await;
}
