use crate::image::{self, Descriptor, Manifest};
use crate::layer::Layer;
use crate::reference::Reference;
use crate::registry::{Registry, ResolvedImage};
use anyhow::Result;
use bytes::Bytes;
use futures::future::try_join_all;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub const EPOCH: &str = "1970-01-01T00:00:00Z";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ConfigPatch {
    #[serde(default)]
    pub env: Vec<(String, String)>,
    #[serde(default)]
    pub entrypoint: Option<Vec<String>>,
    #[serde(default)]
    pub cmd: Option<Vec<String>>,
    #[serde(default)]
    pub workdir: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub exposed_ports: Vec<u16>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

pub struct Assembled {
    pub manifest: Bytes,
    pub manifest_digest: String,
    pub config: Bytes,
    pub config_digest: String,
}

pub fn scratch_config(platform: &image::Platform) -> Value {
    json!({
        "architecture": platform.architecture,
        "os": platform.os,
        "config": {},
        "rootfs": {"type": "layers", "diff_ids": []},
        "history": []
    })
}

pub fn assemble(base: Option<&ResolvedImage>, layers: &[Layer], patch: &ConfigPatch) -> Result<Assembled> {
    let mut config = match base {
        Some(b) => b.config.clone(),
        None => scratch_config(&image::host_platform()),
    };
    let obj = config.as_object_mut().ok_or_else(|| anyhow::anyhow!("image config is not an object"))?;
    obj.insert("created".into(), json!(EPOCH));
    obj.remove("container");
    obj.remove("container_config");
    obj.remove("docker_version");
    let cfg = obj.entry("config").or_insert_with(|| json!({}));
    if cfg.is_null() {
        *cfg = json!({});
    }
    let cfg = cfg.as_object_mut().unwrap();
    if !patch.env.is_empty() {
        let mut env: Vec<String> = cfg
            .get("Env")
            .and_then(|e| e.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
            .unwrap_or_default();
        for (k, v) in &patch.env {
            let prefix = format!("{k}=");
            env.retain(|e| !e.starts_with(&prefix));
            env.push(format!("{k}={v}"));
        }
        cfg.insert("Env".into(), json!(env));
    }
    if let Some(ep) = &patch.entrypoint {
        if ep.is_empty() {
            cfg.remove("Entrypoint");
        } else {
            cfg.insert("Entrypoint".into(), json!(ep));
        }
    }
    if let Some(c) = &patch.cmd {
        cfg.insert("Cmd".into(), json!(c));
    }
    if let Some(w) = &patch.workdir {
        cfg.insert("WorkingDir".into(), json!(w));
    }
    if let Some(u) = &patch.user {
        cfg.insert("User".into(), json!(u));
    }
    if !patch.exposed_ports.is_empty() {
        let ports = cfg.entry("ExposedPorts").or_insert_with(|| json!({}));
        if let Some(p) = ports.as_object_mut() {
            for port in &patch.exposed_ports {
                p.insert(format!("{port}/tcp"), json!({}));
            }
        }
    }
    if !patch.labels.is_empty() {
        let labels = cfg.entry("Labels").or_insert_with(|| json!({}));
        if labels.is_null() {
            *labels = json!({});
        }
        if let Some(l) = labels.as_object_mut() {
            for (k, v) in &patch.labels {
                l.insert(k.clone(), json!(v));
            }
        }
    }
    let rootfs = obj.entry("rootfs").or_insert_with(|| json!({"type": "layers", "diff_ids": []}));
    let diff_ids = rootfs
        .as_object_mut()
        .unwrap()
        .entry("diff_ids")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .unwrap();
    for l in layers {
        diff_ids.push(json!(l.diff_id.to_oci()));
    }
    let history = obj.entry("history").or_insert_with(|| json!([]));
    if let Some(h) = history.as_array_mut() {
        for l in layers {
            h.push(json!({"created": EPOCH, "created_by": format!("acro: {}", l.comment), "comment": "acropolis"}));
        }
    }
    let config_bytes = Bytes::from(serde_json::to_vec(&config)?);
    let config_digest = acro_store::sha256_bytes(&config_bytes).to_oci();
    let mut descs: Vec<Descriptor> = Vec::new();
    if let Some(b) = base {
        for l in &b.manifest.layers {
            descs.push(Descriptor {
                media_type: image::oci_layer_media_type(&l.media_type),
                digest: l.digest.clone(),
                size: l.size,
                platform: None,
                annotations: None,
                artifact_type: None,
            });
        }
    }
    for l in layers {
        descs.push(Descriptor {
            media_type: l.media_type.clone(),
            digest: l.digest.to_oci(),
            size: l.size,
            platform: None,
            annotations: None,
            artifact_type: None,
        });
    }
    let mut annotations = BTreeMap::new();
    if let Some(b) = base {
        annotations.insert("org.opencontainers.image.base.digest".to_string(), b.manifest_digest.clone());
        let mut name = b.reference.clone();
        name.digest = None;
        annotations.insert("org.opencontainers.image.base.name".to_string(), name.to_string());
    }
    let manifest = Manifest {
        schema_version: 2,
        media_type: Some(image::MT_OCI_MANIFEST.to_string()),
        artifact_type: None,
        config: Descriptor {
            media_type: image::MT_OCI_CONFIG.to_string(),
            digest: config_digest.clone(),
            size: config_bytes.len() as u64,
            platform: None,
            annotations: None,
            artifact_type: None,
        },
        layers: descs,
        subject: None,
        annotations: if annotations.is_empty() { None } else { Some(annotations) },
    };
    let manifest_bytes = Bytes::from(serde_json::to_vec(&manifest)?);
    let manifest_digest = acro_store::sha256_bytes(&manifest_bytes).to_oci();
    Ok(Assembled { manifest: manifest_bytes, manifest_digest, config: config_bytes, config_digest })
}

pub async fn copy_base(reg: &Registry, base: &ResolvedImage, target: &Reference) -> Result<u64> {
    copy_layers(reg, &base.reference, &base.manifest.layers, target).await
}

pub async fn copy_layers(reg: &Registry, src: &Reference, layers: &[Descriptor], target: &Reference) -> Result<u64> {
    let futs = layers.iter().map(|d| async move {
        let copied = reg.copy_blob(src, target, d).await?;
        acro_events::emit(acro_events::Event::Uploaded { what: d.digest.clone(), bytes: d.size, skipped: !copied });
        Ok::<u64, anyhow::Error>(if copied { d.size } else { 0 })
    });
    let sizes = try_join_all(futs).await?;
    Ok(sizes.into_iter().sum())
}

pub async fn push_layers(reg: &Registry, target: &Reference, layers: &[Layer]) -> Result<u64> {
    let futs = layers.iter().map(|l| async move {
        let pushed = reg.push_blob_file(target, &l.digest.to_oci(), l.size, &l.path).await?;
        acro_events::emit(acro_events::Event::Uploaded { what: l.comment.clone(), bytes: l.size, skipped: !pushed });
        Ok::<u64, anyhow::Error>(if pushed { l.size } else { 0 })
    });
    let sizes = try_join_all(futs).await?;
    Ok(sizes.into_iter().sum())
}

pub async fn push_manifest(reg: &Registry, target: &Reference, a: &Assembled) -> Result<String> {
    reg.push_blob_bytes(target, &a.config_digest, a.config.clone()).await?;
    let tag = target.tag.clone().unwrap_or_else(|| a.manifest_digest.clone());
    reg.put_manifest(target, &tag, a.manifest.clone(), image::MT_OCI_MANIFEST).await
}
