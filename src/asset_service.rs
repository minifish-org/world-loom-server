use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;
use valence::prelude::Resource;

use crate::persistence::{PersistenceRuntime, StoredAssetDefinition, StoredAssetInstance};
use crate::sculpt_spec::{
    parse_sculpt_spec, validate_sculpt_spec, SculptBudget, SculptConflict, SculptSpec,
};
use crate::world_command::WorldBounds;

pub const ASSET_PROTOCOL_VERSION: u32 = 1;
pub const ASSET_CHANNEL: &str = "world-loom:assets-v1";
pub const ASSET_READY_CHANNEL: &str = "world-loom:assets-ready-v1";
pub const MAX_ASSET_PAYLOAD_BYTES: usize = 65_536;
const MAX_CATALOG_SNAPSHOT_BYTES: usize = 60_000;
const MAX_INSTANCE_STATE_BYTES: usize = 4_096;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssetCatalogEntry {
    pub asset_id: String,
    pub version: u32,
    pub spec_hash: String,
    pub spec: SculptSpec,
    pub budget: SculptBudget,
    pub actor: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssetInstance {
    pub instance_id: String,
    pub asset_id: String,
    pub version: u32,
    pub position: [f64; 3],
    pub rotation_degrees: [f64; 3],
    pub scale: [f64; 3],
    pub collision: Value,
    pub interaction_state: Value,
    pub owner: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SculptValidationReport {
    pub valid: bool,
    pub asset_id: String,
    pub version: u32,
    pub spec_hash: String,
    pub budget: SculptBudget,
    pub conflicts: Vec<SculptConflict>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AssetProtocolEvent {
    CatalogSnapshot {
        assets: Vec<AssetCatalogEntry>,
        instances: Vec<AssetInstance>,
    },
    AssetPublished {
        asset: AssetCatalogEntry,
    },
    SpawnInstance {
        instance: AssetInstance,
    },
    UpdateInstance {
        instance: AssetInstance,
    },
    RemoveInstance {
        instance_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AssetProtocolEnvelope<'a> {
    pub protocol_version: u32,
    #[serde(flatten)]
    pub payload: &'a AssetProtocolEvent,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpawnAssetRequest {
    pub asset_id: String,
    pub version: u32,
    pub idempotency_key: String,
    pub position: [f64; 3],
    pub rotation_degrees: [f64; 3],
    pub scale: [f64; 3],
    #[serde(default = "empty_object")]
    pub interaction_state: Value,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateAssetRequest {
    pub instance_id: String,
    pub position: [f64; 3],
    pub rotation_degrees: [f64; 3],
    pub scale: [f64; 3],
    #[serde(default = "empty_object")]
    pub interaction_state: Value,
}

pub struct AssetService {
    assets: BTreeMap<(String, u32), AssetCatalogEntry>,
    instances: BTreeMap<String, AssetInstance>,
    pending_events: Vec<AssetProtocolEvent>,
}

impl Resource for AssetService {}

impl std::fmt::Debug for AssetService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AssetService")
            .field("assets", &self.assets.len())
            .field("instances", &self.instances.len())
            .field("pending_events", &self.pending_events.len())
            .finish()
    }
}

impl AssetService {
    pub fn load(persistence: &PersistenceRuntime) -> Result<Self, String> {
        let mut assets = BTreeMap::new();
        for stored in persistence
            .list_asset_definitions()
            .map_err(|error| format!("load asset catalog failed: {error}"))?
        {
            let entry = catalog_entry_from_stored(stored)?;
            assets.insert((entry.asset_id.clone(), entry.version), entry);
        }
        let mut instances = BTreeMap::new();
        for stored in persistence
            .list_asset_instances()
            .map_err(|error| format!("load asset instances failed: {error}"))?
        {
            let instance = instance_from_stored(stored)?;
            if !assets.contains_key(&(instance.asset_id.clone(), instance.version)) {
                return Err(format!(
                    "persisted instance {} references missing asset {}@{}",
                    instance.instance_id, instance.asset_id, instance.version
                ));
            }
            instances.insert(instance.instance_id.clone(), instance);
        }
        let service = Self {
            assets,
            instances,
            pending_events: Vec::new(),
        };
        service.ensure_snapshot_budget()?;
        Ok(service)
    }

    pub fn validate_spec_value(value: &Value) -> Result<SculptValidationReport, String> {
        let spec = parse_sculpt_spec(value)?;
        let validated = validate_sculpt_spec(spec);
        let warnings = if validated.budget.estimated_triangles > 40_000 {
            vec!["asset is near the 50,000-triangle limit".to_string()]
        } else {
            Vec::new()
        };
        Ok(SculptValidationReport {
            valid: validated.is_valid(),
            asset_id: validated.spec.asset_id.clone(),
            version: validated.spec.version,
            spec_hash: validated.spec_hash,
            budget: validated.budget,
            conflicts: validated.conflicts,
            warnings,
        })
    }

    pub fn publish(
        &mut self,
        persistence: &PersistenceRuntime,
        value: &Value,
    ) -> Result<AssetCatalogEntry, String> {
        let spec = parse_sculpt_spec(value)?;
        let validated = validate_sculpt_spec(spec);
        if !validated.is_valid() {
            return Err(format!(
                "SculptSpec validation failed: {}",
                serde_json::to_string(&validated.conflicts)
                    .expect("SculptSpec conflicts serialize")
            ));
        }
        let key = (validated.spec.asset_id.clone(), validated.spec.version);
        if let Some(existing) = self.assets.get(&key) {
            if existing.spec_hash == validated.spec_hash {
                return Ok(existing.clone());
            }
            return Err(format!(
                "asset {}@{} already exists with a different spec_hash",
                key.0, key.1
            ));
        }

        let prospective = AssetCatalogEntry {
            asset_id: validated.spec.asset_id.clone(),
            version: validated.spec.version,
            spec_hash: validated.spec_hash.clone(),
            spec: validated.spec.clone(),
            budget: validated.budget.clone(),
            actor: "mcp".to_string(),
            created_at: String::new(),
        };
        let mut prospective_assets = self.list_assets();
        prospective_assets.push(prospective);
        ensure_snapshot_budget(prospective_assets, self.list_instances())?;

        let budget_json = serde_json::to_string(&validated.budget)
            .map_err(|error| format!("serialize asset budget failed: {error}"))?;
        let stored = persistence
            .persist_asset_definition(
                &validated.spec.asset_id,
                validated.spec.version,
                &validated.spec_hash,
                &validated.canonical_json,
                &budget_json,
                "mcp",
            )
            .map_err(|error| format!("persist asset definition failed: {error}"))?;
        let entry = catalog_entry_from_stored(stored)?;
        self.assets.insert(key, entry.clone());
        self.pending_events
            .push(AssetProtocolEvent::AssetPublished {
                asset: entry.clone(),
            });
        Ok(entry)
    }

    pub fn list_assets(&self) -> Vec<AssetCatalogEntry> {
        self.assets.values().cloned().collect()
    }

    pub fn get_asset(&self, asset_id: &str, version: Option<u32>) -> Option<AssetCatalogEntry> {
        if let Some(version) = version {
            return self.assets.get(&(asset_id.to_string(), version)).cloned();
        }
        self.assets
            .range((asset_id.to_string(), 0)..=(asset_id.to_string(), u32::MAX))
            .next_back()
            .map(|(_, asset)| asset.clone())
    }

    pub fn spawn(
        &mut self,
        persistence: &PersistenceRuntime,
        bounds: WorldBounds,
        request: SpawnAssetRequest,
    ) -> Result<AssetInstance, String> {
        validate_instance_key(&request.idempotency_key)?;
        validate_transform(
            bounds,
            request.position,
            request.rotation_degrees,
            request.scale,
        )?;
        validate_state(&request.interaction_state)?;
        let asset = self
            .assets
            .get(&(request.asset_id.clone(), request.version))
            .ok_or_else(|| format!("unknown asset {}@{}", request.asset_id, request.version))?;

        if let Some(stored) = persistence
            .load_asset_instance_by_idempotency_key(&request.idempotency_key)
            .map_err(|error| format!("load asset spawn idempotency record failed: {error}"))?
        {
            let existing = instance_from_stored(stored)?;
            if existing.asset_id == request.asset_id
                && existing.version == request.version
                && existing.position == request.position
                && existing.rotation_degrees == request.rotation_degrees
                && existing.scale == request.scale
                && existing.interaction_state == request.interaction_state
            {
                return Ok(existing);
            }
            return Err(format!(
                "idempotency_key `{}` is already bound to a different asset spawn",
                request.idempotency_key
            ));
        }

        let collision = serde_json::to_value(&asset.spec.collision)
            .map_err(|error| format!("serialize collision metadata failed: {error}"))?;
        let stored = StoredAssetInstance {
            instance_id: Uuid::new_v4().to_string(),
            idempotency_key: request.idempotency_key,
            asset_id: request.asset_id,
            version: request.version,
            position: request.position,
            rotation_degrees: request.rotation_degrees,
            scale: request.scale,
            collision_json: collision.to_string(),
            interaction_state_json: request.interaction_state.to_string(),
            owner: "mcp".to_string(),
            created_at: String::new(),
            updated_at: String::new(),
            removed_at: None,
        };
        let prospective = instance_from_stored(stored.clone())?;
        let mut prospective_instances = self.list_instances();
        prospective_instances.push(prospective);
        ensure_snapshot_budget(self.list_assets(), prospective_instances)?;
        let stored = persistence
            .persist_asset_instance(&stored)
            .map_err(|error| format!("persist asset instance failed: {error}"))?;
        let instance = instance_from_stored(stored)?;
        self.instances
            .insert(instance.instance_id.clone(), instance.clone());
        self.pending_events.push(AssetProtocolEvent::SpawnInstance {
            instance: instance.clone(),
        });
        Ok(instance)
    }

    pub fn update(
        &mut self,
        persistence: &PersistenceRuntime,
        bounds: WorldBounds,
        request: UpdateAssetRequest,
    ) -> Result<AssetInstance, String> {
        validate_transform(
            bounds,
            request.position,
            request.rotation_degrees,
            request.scale,
        )?;
        validate_state(&request.interaction_state)?;
        let current = self
            .instances
            .get(&request.instance_id)
            .cloned()
            .ok_or_else(|| format!("unknown active instance_id `{}`", request.instance_id))?;
        let stored = StoredAssetInstance {
            instance_id: current.instance_id.clone(),
            idempotency_key: persistence
                .load_asset_instance(&current.instance_id)
                .map_err(|error| format!("load asset instance failed: {error}"))?
                .ok_or_else(|| "asset instance metadata is missing".to_string())?
                .idempotency_key,
            asset_id: current.asset_id,
            version: current.version,
            position: request.position,
            rotation_degrees: request.rotation_degrees,
            scale: request.scale,
            collision_json: current.collision.to_string(),
            interaction_state_json: request.interaction_state.to_string(),
            owner: current.owner,
            created_at: current.created_at,
            updated_at: current.updated_at,
            removed_at: None,
        };
        let stored = persistence
            .update_asset_instance(&stored)
            .map_err(|error| format!("persist asset update failed: {error}"))?;
        let instance = instance_from_stored(stored)?;
        self.instances
            .insert(instance.instance_id.clone(), instance.clone());
        self.pending_events
            .push(AssetProtocolEvent::UpdateInstance {
                instance: instance.clone(),
            });
        Ok(instance)
    }

    pub fn remove(
        &mut self,
        persistence: &PersistenceRuntime,
        instance_id: &str,
    ) -> Result<Value, String> {
        let stored = persistence
            .remove_asset_instance(instance_id)
            .map_err(|error| format!("persist asset removal failed: {error}"))?;
        let Some(stored) = stored else {
            return Err(format!("unknown instance_id `{instance_id}`"));
        };
        let already_removed =
            stored.removed_at.is_some() && !self.instances.contains_key(instance_id);
        self.instances.remove(instance_id);
        if !already_removed {
            self.pending_events
                .push(AssetProtocolEvent::RemoveInstance {
                    instance_id: instance_id.to_string(),
                });
        }
        Ok(serde_json::json!({
            "instance_id": instance_id,
            "removed": true,
            "already_removed": already_removed,
        }))
    }

    pub fn list_instances(&self) -> Vec<AssetInstance> {
        self.instances.values().cloned().collect()
    }

    pub fn snapshot_event(&self) -> AssetProtocolEvent {
        AssetProtocolEvent::CatalogSnapshot {
            assets: self.list_assets(),
            instances: self.list_instances(),
        }
    }

    pub fn drain_events(&mut self) -> Vec<AssetProtocolEvent> {
        std::mem::take(&mut self.pending_events)
    }

    fn ensure_snapshot_budget(&self) -> Result<(), String> {
        ensure_snapshot_budget(self.list_assets(), self.list_instances())
    }
}

fn ensure_snapshot_budget(
    assets: Vec<AssetCatalogEntry>,
    instances: Vec<AssetInstance>,
) -> Result<(), String> {
    let bytes = serialize_asset_event(&AssetProtocolEvent::CatalogSnapshot { assets, instances })?;
    if bytes.len() > MAX_CATALOG_SNAPSHOT_BYTES {
        return Err(format!(
            "asset catalog snapshot would be {} bytes; maximum is {MAX_CATALOG_SNAPSHOT_BYTES}",
            bytes.len()
        ));
    }
    Ok(())
}

pub fn parse_spawn_request(arguments: &Value) -> Result<SpawnAssetRequest, String> {
    serde_json::from_value(arguments.clone())
        .map_err(|error| format!("invalid spawn_asset arguments: {error}"))
}

pub fn parse_update_request(arguments: &Value) -> Result<UpdateAssetRequest, String> {
    serde_json::from_value(arguments.clone())
        .map_err(|error| format!("invalid update_asset arguments: {error}"))
}

pub fn serialize_asset_event(event: &AssetProtocolEvent) -> Result<Vec<u8>, String> {
    let bytes = serde_json::to_vec(&AssetProtocolEnvelope {
        protocol_version: ASSET_PROTOCOL_VERSION,
        payload: event,
    })
    .map_err(|error| format!("serialize asset protocol event failed: {error}"))?;
    if bytes.len() > MAX_ASSET_PAYLOAD_BYTES {
        return Err(format!(
            "asset protocol payload is {} bytes; maximum is {MAX_ASSET_PAYLOAD_BYTES}",
            bytes.len()
        ));
    }
    Ok(bytes)
}

fn catalog_entry_from_stored(stored: StoredAssetDefinition) -> Result<AssetCatalogEntry, String> {
    let spec = serde_json::from_str::<SculptSpec>(&stored.canonical_spec_json)
        .map_err(|error| format!("decode stored SculptSpec failed: {error}"))?;
    let validated = validate_sculpt_spec(spec.clone());
    if !validated.is_valid() || validated.spec_hash != stored.spec_hash {
        return Err(format!(
            "stored asset {}@{} failed validation or hash verification",
            stored.asset_id, stored.version
        ));
    }
    let budget = serde_json::from_str(&stored.budget_json)
        .map_err(|error| format!("decode stored asset budget failed: {error}"))?;
    Ok(AssetCatalogEntry {
        asset_id: stored.asset_id,
        version: stored.version,
        spec_hash: stored.spec_hash,
        spec,
        budget,
        actor: stored.actor,
        created_at: stored.created_at,
    })
}

fn instance_from_stored(stored: StoredAssetInstance) -> Result<AssetInstance, String> {
    let collision = serde_json::from_str(&stored.collision_json)
        .map_err(|error| format!("decode stored collision metadata failed: {error}"))?;
    let interaction_state = serde_json::from_str(&stored.interaction_state_json)
        .map_err(|error| format!("decode stored interaction state failed: {error}"))?;
    Ok(AssetInstance {
        instance_id: stored.instance_id,
        asset_id: stored.asset_id,
        version: stored.version,
        position: stored.position,
        rotation_degrees: stored.rotation_degrees,
        scale: stored.scale,
        collision,
        interaction_state,
        owner: stored.owner,
        created_at: stored.created_at,
        updated_at: stored.updated_at,
    })
}

fn validate_instance_key(value: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > 128 {
        Err("idempotency_key must contain 1..=128 UTF-8 bytes".to_string())
    } else {
        Ok(())
    }
}

fn validate_transform(
    bounds: WorldBounds,
    position: [f64; 3],
    rotation: [f64; 3],
    scale: [f64; 3],
) -> Result<(), String> {
    if !position.iter().all(|value| value.is_finite())
        || position[0] < f64::from(bounds.min_x)
        || position[0] > f64::from(bounds.max_x)
        || position[1] < f64::from(bounds.min_y)
        || position[1] > f64::from(bounds.max_y)
        || position[2] < f64::from(bounds.min_z)
        || position[2] > f64::from(bounds.max_z)
    {
        return Err("asset position must be finite and inside active world bounds".to_string());
    }
    if !rotation
        .iter()
        .all(|value| value.is_finite() && (-3600.0..=3600.0).contains(value))
    {
        return Err("asset rotation_degrees must be finite and in [-3600,3600]".to_string());
    }
    if !scale
        .iter()
        .all(|value| value.is_finite() && (0.01..=16.0).contains(value))
    {
        return Err("asset scale must be finite and in [0.01,16]".to_string());
    }
    Ok(())
}

fn validate_state(value: &Value) -> Result<(), String> {
    if !value.is_object() {
        return Err("interaction_state must be a JSON object".to_string());
    }
    let bytes = serde_json::to_vec(value)
        .map_err(|error| format!("serialize interaction_state failed: {error}"))?;
    if bytes.len() > MAX_INSTANCE_STATE_BYTES {
        return Err(format!(
            "interaction_state is {} bytes; maximum is {MAX_INSTANCE_STATE_BYTES}",
            bytes.len()
        ));
    }
    validate_state_value(value, 0)
}

fn validate_state_value(value: &Value, depth: usize) -> Result<(), String> {
    if depth > 4 {
        return Err("interaction_state nesting exceeds 4 levels".to_string());
    }
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) => Ok(()),
        Value::String(value) if value.len() <= 128 => Ok(()),
        Value::String(_) => Err("interaction_state strings are limited to 128 bytes".to_string()),
        Value::Array(values) if values.len() <= 32 => {
            for value in values {
                validate_state_value(value, depth + 1)?;
            }
            Ok(())
        }
        Value::Object(values) if values.len() <= 32 => {
            for (key, value) in values {
                if key.len() > 64 {
                    return Err("interaction_state keys are limited to 64 bytes".to_string());
                }
                validate_state_value(value, depth + 1)?;
            }
            Ok(())
        }
        _ => Err("interaction_state arrays and objects are limited to 32 entries".to_string()),
    }
}

fn empty_object() -> Value {
    serde_json::json!({})
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::fs;
    use std::path::PathBuf;

    fn temp_path(name: &str, suffix: &str) -> PathBuf {
        env::temp_dir().join(format!(
            "world-loom-{name}-{}-{}-{suffix}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn sample_spec() -> Value {
        serde_json::json!({
            "schema_version": 1,
            "asset_id": "restart_prop",
            "version": 1,
            "name": "Restart Prop",
            "materials": [{
                "id": "wood", "base_color": "#885522", "emissive_color": "#000000",
                "metalness": 0.0, "roughness": 0.8, "opacity": 1.0
            }],
            "nodes": [{
                "id": "body", "kind": "box", "material": "wood",
                "position": [0.0, 0.5, 0.0], "rotation_degrees": [0.0, 0.0, 0.0],
                "scale": [1.0, 1.0, 1.0], "size": [1.0, 1.0, 1.0]
            }],
            "collision": {"mode": "bounds"},
            "lod": {"max_distance": 64.0},
            "animations": []
        })
    }

    #[test]
    fn protocol_event_has_version_and_bounded_json() {
        let event = AssetProtocolEvent::CatalogSnapshot {
            assets: Vec::new(),
            instances: Vec::new(),
        };
        let bytes = serialize_asset_event(&event).expect("serialize event");
        let value: Value = serde_json::from_slice(&bytes).expect("parse event");
        assert_eq!(value["protocol_version"], 1);
        assert_eq!(value["event"], "catalog_snapshot");
    }

    #[test]
    fn transform_and_interaction_state_are_bounded() {
        let bounds = crate::world_command::WORLD_BOUNDS;
        assert!(validate_transform(bounds, [10.0, 65.0, 10.0], [0.0; 3], [1.0; 3]).is_ok());
        assert!(validate_transform(bounds, [1000.0, 65.0, 10.0], [0.0; 3], [1.0; 3]).is_err());
        assert!(validate_state(&serde_json::json!({"open": true})).is_ok());
        assert!(validate_state(&Value::String("not an object".to_string())).is_err());
    }

    #[test]
    fn published_asset_and_instance_survive_restart_and_remove_cleanly() {
        let db = temp_path("asset-restart", "world.sqlite3");
        let regions = temp_path("asset-restart", "regions");
        let instance_id;
        {
            let persistence =
                PersistenceRuntime::open_with_region_dir(&db, &regions).expect("open persistence");
            let mut service = AssetService::load(&persistence).expect("load empty service");
            service
                .publish(&persistence, &sample_spec())
                .expect("publish sample");
            let instance = service
                .spawn(
                    &persistence,
                    crate::world_command::WORLD_BOUNDS,
                    SpawnAssetRequest {
                        asset_id: "restart_prop".to_string(),
                        version: 1,
                        idempotency_key: "restart-instance".to_string(),
                        position: [20.0, 65.0, 20.0],
                        rotation_degrees: [0.0; 3],
                        scale: [1.0; 3],
                        interaction_state: serde_json::json!({"lit": true}),
                    },
                )
                .expect("spawn sample");
            instance_id = instance.instance_id;
        }
        {
            let persistence = PersistenceRuntime::open_with_region_dir(&db, &regions)
                .expect("reopen persistence");
            let mut service = AssetService::load(&persistence).expect("reload service");
            assert_eq!(service.list_assets().len(), 1);
            assert_eq!(service.list_instances().len(), 1);
            service
                .remove(&persistence, &instance_id)
                .expect("remove persisted instance");
            let repeated = service
                .remove(&persistence, &instance_id)
                .expect("repeat removal");
            assert_eq!(repeated["already_removed"], true);
        }
        {
            let persistence = PersistenceRuntime::open_with_region_dir(&db, &regions)
                .expect("reopen after removal");
            let service = AssetService::load(&persistence).expect("reload after removal");
            assert_eq!(service.list_assets().len(), 1);
            assert!(service.list_instances().is_empty());
        }
        let _ = fs::remove_file(db);
        let _ = fs::remove_dir_all(regions);
    }
}
