//! The NMOS node (IS-04, IS-05 with the MXL transport, BCP-007-03) on
//! LeeO86's `mxl-nmos`, laid out like mxl-decklink's: one Device; each OMT
//! input is a video and an audio Sender (its MXL flows); each OMT output is
//! a video and an audio Receiver (the MXL flows it sends).

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use mxl_nmos::{
    media_types, ActiveConnection, Component, Device, Flow, Format, MxlDomain, Node, NodeImplementation, Rational,
    Receiver, ReceiverCaps, Role, Sender, Source, StagedConnection,
};
use serde_json::json;
use uuid::Uuid;

use crate::config::Config;
use crate::mxlio::domain;
use crate::reader::Route;
use crate::state::{lock, Shared};
use crate::{input, output};

pub const DEVICE: &str = "omt";

fn video_receiver(config: &Config, key: &str, label: &str) -> Receiver {
    let types = vec![media_types::VIDEO_V210.to_string()];
    let mut r = Receiver::mxl(key, DEVICE, Format::Video, types.clone());
    r.description = format!("{label}: MXL video sent as an OMT source");
    r.label = label.to_string();
    r.caps = ReceiverCaps {
        media_types: types,
        constraint_sets: vec![json!({
            "urn:x-nmos:cap:format:frame_width": {"minimum": 2, "maximum": 3840},
            "urn:x-nmos:cap:format:frame_height": {"minimum": 2, "maximum": 2160},
            "urn:x-nmos:cap:format:interlace_mode": {"enum": ["progressive"]},
        })],
        version: None,
    };
    mxl_nmos::add_group_hint(&mut r.tags, &config.nmos_label, label);
    r
}

fn audio_receiver(config: &Config, key: &str, label: &str) -> Receiver {
    let types = vec![media_types::AUDIO_FLOAT32.to_string()];
    let mut r = Receiver::mxl(key, DEVICE, Format::Audio, types.clone());
    r.description = format!("{label}: MXL audio (float32, 48 kHz) sent with the OMT source");
    r.label = label.to_string();
    r.caps = ReceiverCaps {
        media_types: types,
        constraint_sets: vec![json!({
            "urn:x-nmos:cap:format:channel_count": {"minimum": 1, "maximum": 32},
            "urn:x-nmos:cap:format:sample_rate": {"enum": [{"numerator": 48000, "denominator": 1}]},
            "urn:x-nmos:cap:format:sample_depth": {"enum": [32]}
        })],
        version: None,
    };
    mxl_nmos::add_group_hint(&mut r.tags, &config.nmos_label, label);
    r
}

fn activate_sender(node: &Node, config: &Config, key: &str, flow_id: Uuid) -> Result<(), mxl_nmos::Error> {
    node.set_staged(
        Role::Sender,
        key,
        StagedConnection {
            master_enable: true,
            peer_id: None,
            mxl_domain_id: Some(config.output_domain_id).into(),
            mxl_flow_id: Some(flow_id).into(),
        },
    )?;
    node.set_active(
        Role::Sender,
        key,
        ActiveConnection {
            master_enable: true,
            peer_id: None,
            mxl_domain_id: Some(config.output_domain_id),
            mxl_flow_id: Some(flow_id),
        },
    )
}

/// Receiver keys of output `i`.
fn out_keys(i: usize) -> (String, String) {
    (format!("out-{}-video", i + 1), format!("out-{}-audio", i + 1))
}

pub fn build(config: &Config, shared: Arc<Shared>, connections: PathBuf) -> Result<Node, String> {
    let scan = config.scan_path.clone();
    let own = config.output_domain_dir.clone();
    let n_out = config.outputs.len();
    let implementation = NodeImplementation::new(move |activation| {
        let shared = shared.clone();
        Box::pin(async move {
            if activation.role != Role::Receiver {
                return Ok(());
            }
            for i in 0..n_out {
                let (vk, ak) = out_keys(i);
                let video = activation.key == vk;
                if !video && activation.key != ak {
                    continue;
                }
                let route = match (activation.master_enable, activation.domain_path.clone(), activation.mxl_flow_id) {
                    (true, Some(domain), Some(flow)) => Some(Route { domain, flow }),
                    _ => None,
                };
                tracing::info!("IS-05: {} → {:?}", activation.key, route);
                let slot = &shared.outputs[i];
                if video {
                    *lock(&slot.video_route) = route;
                } else {
                    *lock(&slot.audio_route) = route;
                }
                slot.revision.fetch_add(1, Ordering::AcqRel);
            }
            Ok(())
        })
    })
    .domains(move || {
        domain::scan(&scan, &own)
            .into_iter()
            .map(|d| MxlDomain {
                id: d.id,
                label: d.label,
                path: d.path,
            })
            .collect()
    })
    .on_connections_changed(move |all| {
        if let Err(e) = std::fs::write(&connections, serde_json::to_vec_pretty(&all).unwrap_or_default()) {
            tracing::error!("saving the IS-05 connections failed: {e}");
        }
    });
    let node = Node::new(config.nmos_settings()?, implementation).map_err(|e| e.to_string())?;
    let err = |e: mxl_nmos::Error| e.to_string();
    node.insert_device(Device::new(DEVICE, "OMT bridge").description("MXL ⇄ Open Media Transport (lm-mxl-omt)"))
        .map_err(err)?;
    let f = &config.format;
    for i in 0..config.inputs.len() {
        let (vl, al) = input::labels(config, i);
        // Video
        let key = format!("in-{}-video", i + 1);
        let mut source = Source::video(&key, DEVICE);
        source.label = vl.clone();
        source.grain_rate = Some(Rational::new(f.rate_n, f.rate_d));
        mxl_nmos::add_group_hint(&mut source.tags, &config.nmos_label, &vl);
        node.insert_source(source).map_err(err)?;
        let mut flow = Flow::video(
            &key,
            &key,
            DEVICE,
            media_types::VIDEO_V210,
            Rational::new(f.rate_n, f.rate_d),
            f.width,
            f.height,
            Component::ycbcr_422(f.width, f.height, 10),
        );
        flow.label = vl.clone();
        mxl_nmos::add_group_hint(&mut flow.tags, &config.nmos_label, &vl);
        node.insert_flow(flow).map_err(err)?;
        let flow_id = input::video_flow_id(&config.seed, i);
        let mut sender = Sender::mxl(&key, &key, DEVICE);
        sender.label = vl.clone();
        sender.description = format!("OMT input {} video", i + 1);
        sender.mxl_domain_id = Some(config.output_domain_id);
        sender.mxl_flow_id = Some(flow_id);
        mxl_nmos::add_group_hint(&mut sender.tags, &config.nmos_label, &vl);
        node.insert_sender(sender).map_err(err)?;
        activate_sender(&node, config, &key, flow_id).map_err(err)?;
        // Audio
        let key = format!("in-{}-audio", i + 1);
        let channels: Vec<mxl_nmos::Channel> = (0..config.audio_channels)
            .map(|c| mxl_nmos::Channel::new(format!("Ch {}", c + 1)))
            .collect();
        let mut source = Source::audio(&key, DEVICE, channels);
        source.label = al.clone();
        mxl_nmos::add_group_hint(&mut source.tags, &config.nmos_label, &al);
        node.insert_source(source).map_err(err)?;
        let mut flow = Flow::audio(&key, &key, DEVICE, media_types::AUDIO_FLOAT32, Rational::new(48_000, 1), Some(32));
        flow.label = al.clone();
        mxl_nmos::add_group_hint(&mut flow.tags, &config.nmos_label, &al);
        node.insert_flow(flow).map_err(err)?;
        let flow_id = input::audio_flow_id(&config.seed, i);
        let mut sender = Sender::mxl(&key, &key, DEVICE);
        sender.label = al.clone();
        sender.description = format!("OMT input {} audio", i + 1);
        sender.mxl_domain_id = Some(config.output_domain_id);
        sender.mxl_flow_id = Some(flow_id);
        mxl_nmos::add_group_hint(&mut sender.tags, &config.nmos_label, &al);
        node.insert_sender(sender).map_err(err)?;
        activate_sender(&node, config, &key, flow_id).map_err(err)?;
    }
    for i in 0..config.outputs.len() {
        let (vl, al) = output::labels(config, i);
        let (vk, ak) = out_keys(i);
        node.insert_receiver(video_receiver(config, &vk, &vl)).map_err(err)?;
        node.insert_receiver(audio_receiver(config, &ak, &al)).map_err(err)?;
    }
    Ok(node)
}

/// The saved IS-05 connections of the last run.
pub fn load_connections(path: &std::path::Path) -> Vec<mxl_nmos::SavedConnection> {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}
