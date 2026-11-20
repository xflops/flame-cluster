use flame_operator::api::FlameCluster;
use flame_operator::resources;
use kube::ResourceExt;
use serde::Deserialize;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let file = std::env::args()
        .nth(1)
        .ok_or("usage: render flamecluster.yaml [cluster-name]")?;
    let contents = std::fs::read_to_string(file)?;
    let values = serde_yaml::Deserializer::from_str(&contents)
        .map(serde_json::Value::deserialize)
        .collect::<Result<Vec<_>, _>>()?;
    let cluster: FlameCluster = serde_json::from_value(
        values
            .into_iter()
            .find(|v| v["kind"] == "FlameCluster")
            .ok_or("FlameCluster document missing")?,
    )?;
    let name = std::env::args()
        .nth(2)
        .unwrap_or_else(|| cluster.name_any());
    let namespace = cluster
        .namespace()
        .ok_or("FlameCluster namespace missing")?;
    let spec = &cluster.spec;
    let cache_endpoints = &spec.cache_endpoints;
    let image = resources::session_image(spec);
    // Offline previews do not yet have an API-assigned UID. The operator always
    // renders again using the actual object UID before applying resources.
    for resource in resources::flame_resources(
        &name,
        &namespace,
        &cluster.uid().unwrap_or_else(|| "unassigned-preview".into()),
        spec,
        &image,
        cache_endpoints,
    )? {
        println!("---\n{}", serde_yaml::to_string(&resource)?);
    }
    println!(
        "---\n{}",
        serde_yaml::to_string(&resources::gateway_policy(
            &name,
            &namespace,
            spec,
            cache_endpoints
        ))?
    );
    Ok(())
}
