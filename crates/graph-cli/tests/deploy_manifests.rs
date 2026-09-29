//! The deployment files under `deploy/` parse as YAML and say what
//! `docs/deploy/*.md` promises (ADR 0004 D10, epic story 24). No cluster
//! and no Docker needed: the Compose file runs end to end in the `compose`
//! CI job (`deploy/compose/check.sh`).
//!
//! Every `serve` command line in them is also checked against the real
//! argument parser (`memory-graph serve ... --help` style is not enough, so
//! the flags are compared with `serve --help`'s list).
use std::path::{Path, PathBuf};
use std::process::Command;
use yaml_rust2::{Yaml, YamlLoader};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn load(rel: &str) -> Vec<Yaml> {
    let p = root().join(rel);
    let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    YamlLoader::load_from_str(&text).unwrap_or_else(|e| panic!("{rel}: not YAML: {e}"))
}

fn s(y: &Yaml) -> &str {
    y.as_str().unwrap_or_else(|| panic!("not a string: {y:?}"))
}

fn strings(y: &Yaml) -> Vec<String> {
    y.as_vec()
        .unwrap_or_else(|| panic!("not a list: {y:?}"))
        .iter()
        .map(|x| match x {
            Yaml::String(v) => v.clone(),
            Yaml::Integer(i) => i.to_string(),
            other => panic!("not a scalar: {other:?}"),
        })
        .collect()
}

/// The long flags `memory-graph serve` accepts.
fn serve_flags() -> Vec<String> {
    let o = Command::new(env!("CARGO_BIN_EXE_memory-graph"))
        .args(["serve", "--help"])
        .output()
        .unwrap();
    assert!(o.status.success());
    String::from_utf8_lossy(&o.stdout)
        .split_whitespace()
        .filter(|w| w.starts_with("--"))
        .map(|w| {
            w.trim_end_matches(|c: char| !c.is_ascii_alphanumeric())
                .to_string()
        })
        .collect()
}

fn check_flags(args: &[String], known: &[String], what: &str) {
    assert_eq!(args[0], "serve", "{what}: {args:?}");
    for a in args.iter().filter(|a| a.starts_with("--")) {
        assert!(known.contains(a), "{what}: `{a}` is not a serve flag");
    }
}

fn value_after<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .map(|i| args[i + 1].as_str())
}

#[test]
fn kubernetes_manifests() {
    let known = serve_flags();
    let mut kinds = Vec::new();
    for f in ["service.yaml", "statefulset.yaml", "pdb.yaml"] {
        for doc in load(&format!("deploy/kubernetes/{f}")) {
            let kind = s(&doc["kind"]).to_string();
            assert!(!doc["apiVersion"].is_badvalue(), "{f}: apiVersion");
            let name = s(&doc["metadata"]["name"]).to_string();
            kinds.push((kind.clone(), name.clone()));
            match kind.as_str() {
                "Service" if name == "memory-graph" => {
                    assert_eq!(s(&doc["spec"]["clusterIP"]), "None", "headless");
                    assert_eq!(
                        doc["spec"]["publishNotReadyAddresses"].as_bool(),
                        Some(true)
                    );
                    let ports: Vec<i64> = doc["spec"]["ports"]
                        .as_vec()
                        .unwrap()
                        .iter()
                        .map(|p| p["port"].as_i64().unwrap())
                        .collect();
                    assert!(ports.contains(&7000), "{ports:?}");
                }
                "Service" => assert_eq!(name, "memory-graph-client"),
                "StatefulSet" => {
                    let spec = &doc["spec"];
                    assert_eq!(spec["replicas"].as_i64(), Some(3));
                    assert_eq!(s(&spec["serviceName"]), "memory-graph");
                    assert_eq!(s(&spec["podManagementPolicy"]), "Parallel");
                    let vct = spec["volumeClaimTemplates"].as_vec().unwrap();
                    assert_eq!(s(&vct[0]["metadata"]["name"]), "data");
                    let c = &spec["template"]["spec"]["containers"][0];
                    let args = strings(&c["args"]);
                    check_flags(&args, &known, "statefulset");
                    assert_eq!(value_after(&args, "--data-dir"), Some("/data"));
                    assert!(args.contains(&"--node-id-from-hostname".into()));
                    assert!(args.contains(&"--auto-promote".into()));
                    assert_eq!(
                        value_after(&args, "--advertise"),
                        Some("$(POD_NAME).memory-graph.$(POD_NAMESPACE).svc.cluster.local:7000")
                    );
                    assert!(value_after(&args, "--bootstrap-or-join")
                        .unwrap()
                        .starts_with("memory-graph-0.memory-graph."));
                    let env: Vec<&str> = c["env"]
                        .as_vec()
                        .unwrap()
                        .iter()
                        .map(|e| s(&e["name"]))
                        .collect();
                    assert!(env.contains(&"POD_NAME") && env.contains(&"POD_NAMESPACE"));
                    let ready = &c["readinessProbe"]["grpc"];
                    assert_eq!(ready["port"].as_i64(), Some(7000));
                    assert_eq!(s(&ready["service"]), graph_server_ready());
                    let live = &c["livenessProbe"]["grpc"];
                    assert_eq!(live["port"].as_i64(), Some(7000));
                    assert!(
                        live["service"].is_badvalue(),
                        "liveness: the default service"
                    );
                    assert_eq!(
                        s(&c["volumeMounts"][0]["mountPath"]),
                        "/data",
                        "the claim is mounted at the data dir"
                    );
                    assert_eq!(
                        spec["template"]["spec"]["securityContext"]["fsGroup"].as_i64(),
                        Some(65532),
                        "the image's user owns the volume"
                    );
                    // --replicas tells ordinal 0 whom to ask after a lost
                    // volume: it must match the StatefulSet.
                    assert_eq!(
                        value_after(&args, "--replicas").and_then(|r| r.parse::<i64>().ok()),
                        spec["replicas"].as_i64()
                    );
                    let sc = &c["securityContext"];
                    assert_eq!(sc["allowPrivilegeEscalation"].as_bool(), Some(false));
                    assert_eq!(sc["readOnlyRootFilesystem"].as_bool(), Some(true));
                    assert_eq!(strings(&sc["capabilities"]["drop"]), ["ALL"]);
                    assert_eq!(s(&sc["seccompProfile"]["type"]), "RuntimeDefault");
                    let image = s(&c["image"]);
                    assert!(
                        !image.ends_with(":main") && !image.ends_with(":latest"),
                        "a pinned (placeholder) tag, not a moving one: {image}"
                    );
                }
                "PodDisruptionBudget" => {
                    assert_eq!(doc["spec"]["minAvailable"].as_i64(), Some(2));
                }
                other => panic!("unexpected kind {other}"),
            }
        }
    }
    kinds.sort();
    assert_eq!(
        kinds,
        [
            ("PodDisruptionBudget".into(), "memory-graph".into()),
            ("Service".into(), "memory-graph".into()),
            ("Service".into(), "memory-graph-client".into()),
            ("StatefulSet".into(), "memory-graph".into()),
        ]
    );
}

/// The readiness service name (kept in sync with graph-server's constant
/// without depending on the crate here).
fn graph_server_ready() -> &'static str {
    "memory-graph.ready"
}

#[test]
fn compose_file() {
    let known = serve_flags();
    let docs = load("deploy/compose/cluster.yml");
    assert_eq!(docs.len(), 1);
    let services = &docs[0]["services"];
    for n in 1..=3 {
        let svc = &services[format!("node{n}").as_str()];
        assert!(!svc.is_badvalue(), "node{n}");
        let args = strings(&svc["command"]);
        check_flags(&args, &known, &format!("node{n}"));
        assert_eq!(value_after(&args, "--data-dir"), Some("/data"));
        assert_eq!(value_after(&args, "--listen"), Some("0.0.0.0:7000"));
        let advertise = format!("node{n}:7000");
        assert_eq!(value_after(&args, "--advertise"), Some(advertise.as_str()));
        let id = n.to_string();
        assert_eq!(value_after(&args, "--node-id"), Some(id.as_str()));
        let ports = strings(&svc["ports"]);
        assert!(
            ports.contains(&format!("${{MG_GRPC_PORT_{n}:-700{n}}}:7000")),
            "{ports:?}"
        );
        let volumes = strings(&svc["volumes"]);
        assert_eq!(volumes, [format!("node{n}-data:/data")]);
        if n == 1 {
            assert!(args.contains(&"--bootstrap".into()));
            assert!(svc["depends_on"].is_badvalue());
        } else {
            assert_eq!(value_after(&args, "--join"), Some("node1:7000"));
            assert!(args.contains(&"--auto-promote".into()));
            assert_eq!(
                s(&svc["depends_on"]["node1"]["condition"]),
                "service_healthy"
            );
        }
    }
    // The shared anchor: build context, the healthcheck using --ready.
    let base = &docs[0]["x-node"];
    assert_eq!(s(&base["build"]["context"]), "../..");
    let hc = strings(&base["healthcheck"]["test"]);
    assert_eq!(
        hc,
        [
            "CMD",
            "/memory-graph",
            "health",
            "--ready",
            "--server",
            "127.0.0.1:7000"
        ]
    );
    for n in 1..=3 {
        assert!(!docs[0]["volumes"][format!("node{n}-data").as_str()].is_badvalue());
    }
}
