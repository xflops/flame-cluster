use flame_api::{FlameCluster, FlameWorker};
use kube::CustomResourceExt;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let crd = match std::env::args().nth(1).as_deref() {
        None | Some("flame") => FlameCluster::crd(),
        Some("worker") => FlameWorker::crd(),
        _ => return Err("usage: crdgen [flame|worker]".into()),
    };
    print!("{}", serde_yaml::to_string(&crd)?);
    Ok(())
}
