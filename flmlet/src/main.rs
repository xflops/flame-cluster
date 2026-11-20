use flame_api::{FlameCluster, FlameWorker};
use flmlet::{Input, Settings, host::LocalHost};
use kube::{
    Api, Client, ResourceExt,
    api::{Patch, PatchParams},
    config::{KubeConfigOptions, Kubeconfig},
};
use serde_json::json;
use std::{path::PathBuf, time::Duration};

type Error = Box<dyn std::error::Error + Send + Sync>;
#[derive(Debug)]
struct Cli {
    settings: Settings,
    kubeconfig: Option<PathBuf>,
    once: bool,
    check: bool,
    configure: bool,
}
impl Cli {
    fn parse() -> Result<Option<Self>, Error> {
        let mut cli = Self {
            settings: Settings {
                worker: String::new(),
                namespace: "flame-system".into(),
                prefix: "/usr/local/flame".into(),
                systemd_dir: "/etc/systemd/system".into(),
                adopt_existing: false,
            },
            kubeconfig: None,
            once: false,
            check: false,
            configure: false,
        };
        let mut args = std::env::args().skip(1);
        while let Some(flag) = args.next() {
            match flag.as_str() {
                "--version" => {
                    println!("flmlet {}", env!("CARGO_PKG_VERSION"));
                    return Ok(None);
                }
                "--help" | "-h" => {
                    println!(
                        "flmlet --worker NAME [--namespace NS] [--prefix DIR] [--kubeconfig FILE] [--systemd-dir DIR] [--adopt-existing] [--once] [--check | --configure]\nSREs install Flame binaries and provide scoped Kubernetes credentials. --check prints desired configuration without host mutations."
                    );
                    return Ok(None);
                }
                "--adopt-existing" => cli.settings.adopt_existing = true,
                "--once" => cli.once = true,
                "--check" => cli.check = true,
                "--configure" => cli.configure = true,
                "--worker" | "--namespace" | "--prefix" | "--systemd-dir" | "--kubeconfig" => {
                    let value = args
                        .next()
                        .ok_or_else(|| format!("{flag} requires a value"))?;
                    match flag.as_str() {
                        "--worker" => cli.settings.worker = value,
                        "--namespace" => cli.settings.namespace = value,
                        "--prefix" => cli.settings.prefix = value.into(),
                        "--systemd-dir" => cli.settings.systemd_dir = value.into(),
                        _ => cli.kubeconfig = Some(value.into()),
                    }
                }
                _ => return Err(format!("unknown argument {flag}; see --help").into()),
            }
        }
        if cli.settings.worker.is_empty() {
            return Err("--worker NAME is required".into());
        }
        if cli.check && cli.configure {
            return Err("--check and --configure are mutually exclusive".into());
        }
        cli.settings.validate()?;
        Ok(Some(cli))
    }
}

async fn client(path: Option<&PathBuf>) -> Result<Client, Error> {
    if let Some(path) = path {
        Ok(Client::try_from(
            kube::Config::from_custom_kubeconfig(
                Kubeconfig::read_from(path)?,
                &KubeConfigOptions::default(),
            )
            .await?,
        )?)
    } else {
        Ok(Client::try_default().await?)
    }
}

async fn api_error(cli: &Cli, error: kube::Error) -> Error {
    if matches!(&error, kube::Error::Api(response) if response.code == 404) {
        if !cli.check {
            let settings = cli.settings.clone();
            match tokio::task::spawn_blocking(move || {
                flmlet::reconcile(&mut LocalHost, &settings, Input::Missing)
            })
            .await
            {
                Ok(Err(error)) => return error.into(),
                Err(error) => return error.into(),
                _ => {}
            }
        }
        return "Referenced Kubernetes object was removed; owned services are stopped".into();
    }
    if matches!(&error, kube::Error::Api(response) if matches!(response.code, 401 | 403)) {
        if !cli.check {
            let settings = cli.settings.clone();
            match tokio::task::spawn_blocking(move || {
                flmlet::reconcile(&mut LocalHost, &settings, Input::Unauthorized)
            })
            .await
            {
                Ok(Err(error)) => return error.into(),
                Err(error) => return error.into(),
                _ => {}
            }
        }
        return flmlet::Error::Unauthorized.into();
    }
    error.into()
}
async fn patch_status(
    cli: &Cli,
    workers: &Api<FlameWorker>,
    worker: &FlameWorker,
    status: flame_api::FlameWorkerStatus,
) -> Result<(), Error> {
    if let Err(error) = workers
        .patch_status(
            &worker.name_any(),
            &PatchParams::default(),
            &Patch::Merge(json!({"status":status})),
        )
        .await
    {
        return Err(api_error(cli, error).await);
    }
    Ok(())
}

async fn cycle(cli: &Cli, client: Client) -> Result<bool, Error> {
    let workers = Api::<FlameWorker>::namespaced(client.clone(), &cli.settings.namespace);
    let worker = match workers.get_opt(&cli.settings.worker).await {
        Ok(worker) => worker,
        Err(error) => return Err(api_error(cli, error).await),
    };
    let Some(worker) = worker else {
        if cli.check || cli.configure {
            return Err(
                "selected FlameWorker does not exist; installation preflight cannot proceed".into(),
            );
        }
        let settings = cli.settings.clone();
        tokio::task::spawn_blocking(move || {
            flmlet::reconcile(&mut LocalHost, &settings, Input::Missing)
        })
        .await??;
        return Ok(true);
    };
    let disabled = worker.spec.disabled || worker.metadata.deletion_timestamp.is_some();
    let cluster = if disabled {
        None
    } else {
        Some(
            match Api::<FlameCluster>::namespaced(client, &cli.settings.namespace)
                .get(&worker.spec.cluster_ref)
                .await
            {
                Ok(cluster) => cluster,
                Err(error) => return Err(api_error(cli, error).await),
            },
        )
    };
    if cli.configure {
        let cluster = cluster.ok_or("cannot configure a disabled worker")?;
        let settings = cli.settings.clone();
        tokio::task::spawn_blocking(move || {
            flmlet::configure(&mut LocalHost, &settings, &worker, &cluster)
        })
        .await??;
        println!(
            "Configured {}/{}; services will be reconciled by the flmlet daemon.",
            cli.settings.namespace, cli.settings.worker
        );
        return Ok(true);
    }
    if cli.check {
        if let Some(cluster) = cluster {
            let settings = cli.settings.clone();
            let preview = tokio::task::spawn_blocking(move || {
                flmlet::preflight(&mut LocalHost, &settings, &worker, &cluster)
            })
            .await??;
            println!("{}", serde_json::to_string_pretty(&preview)?);
        } else {
            println!(
                "{}",
                json!({"worker":cli.settings.worker,"action":"stop owned services","reason":"worker disabled or deleting"})
            );
        }
        return Ok(true);
    }
    let settings = cli.settings.clone();
    let observed = worker.clone();
    let result = tokio::task::spawn_blocking(move || {
        flmlet::reconcile(
            &mut LocalHost,
            &settings,
            Input::Present {
                worker: &observed,
                cluster: cluster.as_ref(),
            },
        )
    })
    .await?;
    let (status, ready) = match result {
        Ok(outcome) => (outcome.status, outcome.ready || disabled),
        Err(error) => {
            let status = flmlet::failed_status(&worker, &error.to_string());
            patch_status(cli, &workers, &worker, status).await?;
            return Err(error.into());
        }
    };
    if let Some(status) = status {
        patch_status(cli, &workers, &worker, status).await?;
    }
    Ok(ready)
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let Some(cli) = Cli::parse()? else {
        return Ok(());
    };
    let client = client(cli.kubeconfig.as_ref()).await?;
    loop {
        let result = cycle(&cli, client.clone()).await;
        if cli.once || cli.check || cli.configure {
            if result? {
                return Ok(());
            }
            return Err("worker is not Ready after reconciliation".into());
        }
        if let Err(error) = result {
            tracing::error!(%error,"worker reconciliation failed; existing configuration is retained");
        }
        tokio::select! {_=tokio::time::sleep(Duration::from_secs(10))=>{},_=tokio::signal::ctrl_c()=>break}
    }
    Ok(())
}
