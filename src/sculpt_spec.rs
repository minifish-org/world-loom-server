use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const SCULPT_SPEC_SCHEMA_VERSION: u32 = 1;
pub const MAX_SCULPT_SPEC_BYTES: usize = 65_536;
pub const MAX_SCULPT_NODES: usize = 128;
pub const MAX_SCULPT_MATERIALS: usize = 32;
pub const MAX_SCULPT_ANIMATIONS: usize = 32;
pub const MAX_SCULPT_PRIMITIVES: usize = 256;
pub const MAX_SCULPT_TRIANGLES: usize = 50_000;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SculptSpec {
    pub schema_version: u32,
    pub asset_id: String,
    pub version: u32,
    pub name: String,
    pub materials: Vec<SculptMaterial>,
    pub nodes: Vec<SculptNode>,
    pub collision: SculptCollision,
    pub lod: SculptLod,
    pub animations: Vec<SculptAnimation>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SculptMaterial {
    pub id: String,
    pub base_color: String,
    pub emissive_color: String,
    pub metalness: f64,
    pub roughness: f64,
    pub opacity: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SculptNode {
    pub id: String,
    #[serde(default)]
    pub parent: Option<String>,
    pub kind: String,
    #[serde(default)]
    pub material: Option<String>,
    pub position: [f64; 3],
    pub rotation_degrees: [f64; 3],
    pub scale: [f64; 3],
    #[serde(default)]
    pub size: Option<Vec<f64>>,
    #[serde(default)]
    pub radius: Option<f64>,
    #[serde(default)]
    pub radius_top: Option<f64>,
    #[serde(default)]
    pub radius_bottom: Option<f64>,
    #[serde(default)]
    pub height: Option<f64>,
    #[serde(default)]
    pub segments: Option<u32>,
    #[serde(default)]
    pub repeat: Option<SculptRepeat>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SculptRepeat {
    pub count: u32,
    pub offset: [f64; 3],
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SculptCollision {
    pub mode: String,
    #[serde(default)]
    pub boxes: Vec<SculptCollisionBox>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SculptCollisionBox {
    pub center: [f64; 3],
    pub size: [f64; 3],
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SculptLod {
    pub max_distance: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SculptAnimation {
    pub node: String,
    pub kind: String,
    pub axis: String,
    pub speed: f64,
    pub amplitude: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SculptBudget {
    pub source_bytes: usize,
    pub nodes: usize,
    pub expanded_primitives: usize,
    pub estimated_triangles: usize,
    pub estimated_draw_calls: usize,
    pub materials: usize,
    pub animations: usize,
    pub textures: usize,
    pub bounds: Option<SculptBounds>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SculptBounds {
    pub min: SculptVec3,
    pub max: SculptVec3,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SculptVec3 {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SculptConflict {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ValidatedSculptSpec {
    pub spec: SculptSpec,
    pub canonical_json: String,
    pub spec_hash: String,
    pub budget: SculptBudget,
    pub conflicts: Vec<SculptConflict>,
}

impl ValidatedSculptSpec {
    pub fn is_valid(&self) -> bool {
        self.conflicts.is_empty()
    }
}

pub fn parse_sculpt_spec(value: &serde_json::Value) -> Result<SculptSpec, String> {
    serde_json::from_value(value.clone()).map_err(|error| format!("invalid SculptSpec v1: {error}"))
}

pub fn validate_sculpt_spec(spec: SculptSpec) -> ValidatedSculptSpec {
    let canonical_json =
        serde_json::to_string(&spec).expect("SculptSpec serialization cannot fail");
    let spec_hash = format!("{:x}", Sha256::digest(canonical_json.as_bytes()));
    let mut conflicts = Vec::new();
    let mut material_ids = BTreeSet::new();
    let mut node_ids = BTreeSet::new();
    let mut world_positions = BTreeMap::<String, [f64; 3]>::new();
    let mut expanded_primitives = 0usize;
    let mut estimated_triangles = 0usize;
    let mut bounds: Option<SculptBounds> = None;
    let animation_count = spec.animations.len();

    if spec.schema_version != SCULPT_SPEC_SCHEMA_VERSION {
        push(
            &mut conflicts,
            "unsupported_schema_version",
            format!("schema_version {} is unsupported", spec.schema_version),
            None,
        );
    }
    if !valid_id(&spec.asset_id) {
        push(
            &mut conflicts,
            "invalid_asset_id",
            "asset_id must match ^[a-z][a-z0-9_-]{0,63}$",
            None,
        );
    }
    if !(1..=65_535).contains(&spec.version) {
        push(
            &mut conflicts,
            "invalid_version",
            "version must be in 1..=65535",
            None,
        );
    }
    if spec.name.is_empty() || spec.name.len() > 80 {
        push(
            &mut conflicts,
            "invalid_name",
            "name must contain 1..=80 UTF-8 bytes",
            None,
        );
    }
    if canonical_json.len() > MAX_SCULPT_SPEC_BYTES {
        push(
            &mut conflicts,
            "source_size_limit",
            format!(
                "canonical spec is {} bytes; maximum is {MAX_SCULPT_SPEC_BYTES}",
                canonical_json.len()
            ),
            None,
        );
    }
    if spec.materials.is_empty() || spec.materials.len() > MAX_SCULPT_MATERIALS {
        push(
            &mut conflicts,
            "material_limit",
            format!("materials must contain 1..={MAX_SCULPT_MATERIALS} entries"),
            None,
        );
    }
    for material in &spec.materials {
        if !valid_id(&material.id) || !material_ids.insert(material.id.clone()) {
            push(
                &mut conflicts,
                "invalid_material",
                format!("material id `{}` is invalid or duplicated", material.id),
                None,
            );
        }
        if !valid_color(&material.base_color) || !valid_color(&material.emissive_color) {
            push(
                &mut conflicts,
                "invalid_color",
                format!("material `{}` colors must be #RRGGBB", material.id),
                None,
            );
        }
        for (name, value) in [
            ("metalness", material.metalness),
            ("roughness", material.roughness),
            ("opacity", material.opacity),
        ] {
            if !finite_range(value, 0.0, 1.0) {
                push(
                    &mut conflicts,
                    "invalid_material_value",
                    format!(
                        "material `{}` {name} must be finite and in [0,1]",
                        material.id
                    ),
                    None,
                );
            }
        }
    }

    if spec.nodes.is_empty() || spec.nodes.len() > MAX_SCULPT_NODES {
        push(
            &mut conflicts,
            "node_limit",
            format!("nodes must contain 1..={MAX_SCULPT_NODES} entries"),
            None,
        );
    }
    for node in &spec.nodes {
        if !valid_id(&node.id) || !node_ids.insert(node.id.clone()) {
            push(
                &mut conflicts,
                "invalid_node_id",
                format!("node id `{}` is invalid or duplicated", node.id),
                Some(&node.id),
            );
            continue;
        }
        if let Some(parent) = &node.parent {
            if !world_positions.contains_key(parent) {
                push(
                    &mut conflicts,
                    "invalid_parent",
                    format!("parent `{parent}` must reference an earlier node"),
                    Some(&node.id),
                );
            }
        }
        if !vector_range(node.position, -64.0, 64.0) {
            push(
                &mut conflicts,
                "invalid_position",
                "position components must be finite and in [-64,64]",
                Some(&node.id),
            );
        }
        if !vector_range(node.rotation_degrees, -3600.0, 3600.0) {
            push(
                &mut conflicts,
                "invalid_rotation",
                "rotation components must be finite and in [-3600,3600]",
                Some(&node.id),
            );
        }
        if !vector_range(node.scale, 0.01, 16.0) {
            push(
                &mut conflicts,
                "invalid_scale",
                "scale components must be finite and in [0.01,16]",
                Some(&node.id),
            );
        }
        let parent_position = node
            .parent
            .as_ref()
            .and_then(|parent| world_positions.get(parent))
            .copied()
            .unwrap_or([0.0; 3]);
        let world_position = [
            parent_position[0] + node.position[0],
            parent_position[1] + node.position[1],
            parent_position[2] + node.position[2],
        ];
        world_positions.insert(node.id.clone(), world_position);

        let repeat_count = node.repeat.as_ref().map(|repeat| repeat.count).unwrap_or(1);
        if !(1..=32).contains(&repeat_count) {
            push(
                &mut conflicts,
                "repeat_limit",
                "repeat.count must be in 1..=32",
                Some(&node.id),
            );
        }
        if let Some(repeat) = &node.repeat {
            if !vector_range(repeat.offset, -64.0, 64.0) {
                push(
                    &mut conflicts,
                    "invalid_repeat_offset",
                    "repeat.offset must be finite and in [-64,64]",
                    Some(&node.id),
                );
            }
        }

        let geometry = validate_node_geometry(node, &material_ids, &mut conflicts);
        if let Some((triangles, half_size)) = geometry {
            expanded_primitives = expanded_primitives.saturating_add(repeat_count as usize);
            estimated_triangles =
                estimated_triangles.saturating_add(triangles.saturating_mul(repeat_count as usize));
            let offset = node
                .repeat
                .as_ref()
                .map(|repeat| repeat.offset)
                .unwrap_or([0.0; 3]);
            for index in 0..repeat_count {
                let center = [
                    world_position[0] + offset[0] * f64::from(index),
                    world_position[1] + offset[1] * f64::from(index),
                    world_position[2] + offset[2] * f64::from(index),
                ];
                let scaled_half = [
                    half_size[0] * node.scale[0],
                    half_size[1] * node.scale[1],
                    half_size[2] * node.scale[2],
                ];
                extend_bounds(&mut bounds, center, scaled_half);
            }
        }
    }

    if expanded_primitives > MAX_SCULPT_PRIMITIVES {
        push(
            &mut conflicts,
            "primitive_limit",
            format!("expanded primitives {expanded_primitives} exceed {MAX_SCULPT_PRIMITIVES}"),
            None,
        );
    }
    if estimated_triangles > MAX_SCULPT_TRIANGLES {
        push(
            &mut conflicts,
            "triangle_limit",
            format!("estimated triangles {estimated_triangles} exceed {MAX_SCULPT_TRIANGLES}"),
            None,
        );
    }
    validate_collision(&spec.collision, &mut conflicts);
    if !finite_range(spec.lod.max_distance, 8.0, 256.0) {
        push(
            &mut conflicts,
            "invalid_lod",
            "lod.max_distance must be finite and in [8,256]",
            None,
        );
    }
    if spec.animations.len() > MAX_SCULPT_ANIMATIONS {
        push(
            &mut conflicts,
            "animation_limit",
            format!("animations exceed {MAX_SCULPT_ANIMATIONS}"),
            None,
        );
    }
    for animation in &spec.animations {
        if !node_ids.contains(&animation.node)
            || !matches!(animation.kind.as_str(), "rotate" | "bob")
            || !matches!(animation.axis.as_str(), "x" | "y" | "z")
            || !finite_range(animation.speed, -10.0, 10.0)
            || !finite_range(animation.amplitude, 0.0, 16.0)
        {
            push(
                &mut conflicts,
                "invalid_animation",
                format!(
                    "animation for `{}` has invalid target or bounded parameters",
                    animation.node
                ),
                Some(&animation.node),
            );
        }
    }

    ValidatedSculptSpec {
        spec,
        canonical_json: canonical_json.clone(),
        spec_hash,
        budget: SculptBudget {
            source_bytes: canonical_json.len(),
            nodes: node_ids.len(),
            expanded_primitives,
            estimated_triangles,
            estimated_draw_calls: expanded_primitives,
            materials: material_ids.len(),
            animations: animation_count,
            textures: 0,
            bounds,
        },
        conflicts,
    }
}

fn validate_node_geometry(
    node: &SculptNode,
    materials: &BTreeSet<String>,
    conflicts: &mut Vec<SculptConflict>,
) -> Option<(usize, [f64; 3])> {
    if node.kind == "group" {
        if node.material.is_some()
            || node.size.is_some()
            || node.radius.is_some()
            || node.radius_top.is_some()
            || node.radius_bottom.is_some()
            || node.height.is_some()
            || node.segments.is_some()
        {
            push(
                conflicts,
                "invalid_group",
                "group nodes cannot declare material or geometry fields",
                Some(&node.id),
            );
        }
        return None;
    }
    let Some(material) = &node.material else {
        push(
            conflicts,
            "missing_material",
            "primitive node requires material",
            Some(&node.id),
        );
        return None;
    };
    if !materials.contains(material) {
        push(
            conflicts,
            "unknown_material",
            format!("material `{material}` is not defined"),
            Some(&node.id),
        );
    }
    let invalid = |message: &str, conflicts: &mut Vec<SculptConflict>| {
        push(conflicts, "invalid_geometry", message, Some(&node.id));
        None
    };
    match node.kind.as_str() {
        "box" => match node.size.as_deref() {
            Some([x, y, z]) if dimensions(&[*x, *y, *z]) && no_round_fields(node) => {
                Some((12, [x / 2.0, y / 2.0, z / 2.0]))
            }
            _ => invalid("box requires only a positive 3-component size", conflicts),
        },
        "plane" => match node.size.as_deref() {
            Some([x, y]) if dimensions(&[*x, *y]) && no_round_fields(node) => {
                Some((2, [x / 2.0, y / 2.0, 0.005]))
            }
            _ => invalid("plane requires only a positive 2-component size", conflicts),
        },
        "sphere" => match (node.radius, node.segments) {
            (Some(radius), Some(segments))
                if dimension(radius)
                    && valid_segments(segments)
                    && node.size.is_none()
                    && node.height.is_none()
                    && node.radius_top.is_none()
                    && node.radius_bottom.is_none() =>
            {
                Some(((2 * segments * segments.div_ceil(2)) as usize, [radius; 3]))
            }
            _ => invalid("sphere requires radius and segments only", conflicts),
        },
        "cylinder" => match (
            node.radius_top,
            node.radius_bottom,
            node.height,
            node.segments,
        ) {
            (Some(top), Some(bottom), Some(height), Some(segments))
                if dimension(top)
                    && dimension(bottom)
                    && dimension(height)
                    && valid_segments(segments)
                    && node.size.is_none()
                    && node.radius.is_none() =>
            {
                Some((
                    (4 * segments) as usize,
                    [top.max(bottom), height / 2.0, top.max(bottom)],
                ))
            }
            _ => invalid(
                "cylinder requires radius_top, radius_bottom, height, and segments",
                conflicts,
            ),
        },
        "cone" => match (node.radius, node.height, node.segments) {
            (Some(radius), Some(height), Some(segments))
                if dimension(radius)
                    && dimension(height)
                    && valid_segments(segments)
                    && node.size.is_none()
                    && node.radius_top.is_none()
                    && node.radius_bottom.is_none() =>
            {
                Some(((2 * segments) as usize, [radius, height / 2.0, radius]))
            }
            _ => invalid("cone requires radius, height, and segments", conflicts),
        },
        _ => invalid("unsupported node kind", conflicts),
    }
}

fn validate_collision(collision: &SculptCollision, conflicts: &mut Vec<SculptConflict>) {
    match collision.mode.as_str() {
        "none" | "bounds" if collision.boxes.is_empty() => {}
        "compound_boxes" if (1..=16).contains(&collision.boxes.len()) => {
            for item in &collision.boxes {
                if !vector_range(item.center, -64.0, 64.0) || !vector_range(item.size, 0.01, 64.0) {
                    push(
                        conflicts,
                        "invalid_collision",
                        "collision boxes must use bounded centers and positive sizes",
                        None,
                    );
                }
            }
        }
        _ => push(
            conflicts,
            "invalid_collision",
            "collision mode/boxes combination is invalid",
            None,
        ),
    }
}

fn extend_bounds(bounds: &mut Option<SculptBounds>, center: [f64; 3], half: [f64; 3]) {
    let min = SculptVec3 {
        x: center[0] - half[0],
        y: center[1] - half[1],
        z: center[2] - half[2],
    };
    let max = SculptVec3 {
        x: center[0] + half[0],
        y: center[1] + half[1],
        z: center[2] + half[2],
    };
    match bounds {
        Some(bounds) => {
            bounds.min.x = bounds.min.x.min(min.x);
            bounds.min.y = bounds.min.y.min(min.y);
            bounds.min.z = bounds.min.z.min(min.z);
            bounds.max.x = bounds.max.x.max(max.x);
            bounds.max.y = bounds.max.y.max(max.y);
            bounds.max.z = bounds.max.z.max(max.z);
        }
        None => *bounds = Some(SculptBounds { min, max }),
    }
}

fn push(
    conflicts: &mut Vec<SculptConflict>,
    code: &str,
    message: impl Into<String>,
    node: Option<&str>,
) {
    conflicts.push(SculptConflict {
        code: code.to_string(),
        message: message.into(),
        node: node.map(str::to_string),
    });
}

fn valid_id(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some('a'..='z'))
        && value.len() <= 64
        && chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '_' | '-'))
}

fn valid_color(value: &str) -> bool {
    value.len() == 7
        && value.starts_with('#')
        && value[1..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn finite_range(value: f64, min: f64, max: f64) -> bool {
    value.is_finite() && (min..=max).contains(&value)
}
fn vector_range(value: [f64; 3], min: f64, max: f64) -> bool {
    value.into_iter().all(|item| finite_range(item, min, max))
}
fn dimension(value: f64) -> bool {
    finite_range(value, 0.01, 64.0)
}
fn dimensions(values: &[f64]) -> bool {
    values.iter().copied().all(dimension)
}
fn valid_segments(value: u32) -> bool {
    (3..=32).contains(&value)
}
fn no_round_fields(node: &SculptNode) -> bool {
    node.radius.is_none()
        && node.radius_top.is_none()
        && node.radius_bottom.is_none()
        && node.height.is_none()
        && node.segments.is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> SculptSpec {
        SculptSpec {
            schema_version: 1,
            asset_id: "test_prop".to_string(),
            version: 1,
            name: "Test Prop".to_string(),
            materials: vec![SculptMaterial {
                id: "wood".to_string(),
                base_color: "#885522".to_string(),
                emissive_color: "#000000".to_string(),
                metalness: 0.0,
                roughness: 0.8,
                opacity: 1.0,
            }],
            nodes: vec![SculptNode {
                id: "body".to_string(),
                parent: None,
                kind: "box".to_string(),
                material: Some("wood".to_string()),
                position: [0.0, 0.5, 0.0],
                rotation_degrees: [0.0; 3],
                scale: [1.0; 3],
                size: Some(vec![1.0, 1.0, 1.0]),
                radius: None,
                radius_top: None,
                radius_bottom: None,
                height: None,
                segments: None,
                repeat: None,
            }],
            collision: SculptCollision {
                mode: "bounds".to_string(),
                boxes: vec![],
            },
            lod: SculptLod { max_distance: 64.0 },
            animations: vec![],
        }
    }

    #[test]
    fn valid_box_reports_repeatable_budget_and_bounds() {
        let validated = validate_sculpt_spec(sample());
        assert!(validated.is_valid(), "{:?}", validated.conflicts);
        assert_eq!(validated.budget.expanded_primitives, 1);
        assert_eq!(validated.budget.estimated_triangles, 12);
        assert!(validated.budget.bounds.is_some());
        assert_eq!(validated.spec_hash.len(), 64);
    }

    #[test]
    fn executable_or_url_fields_are_rejected_by_schema() {
        let mut value = serde_json::to_value(sample()).unwrap();
        value["script"] = serde_json::json!("alert(1)");
        assert!(parse_sculpt_spec(&value).is_err());
        value.as_object_mut().unwrap().remove("script");
        value["materials"][0]["texture_url"] = serde_json::json!("https://example.test/a.png");
        assert!(parse_sculpt_spec(&value).is_err());
    }

    #[test]
    fn forward_parent_and_over_budget_repeat_are_rejected() {
        let mut spec = sample();
        spec.nodes[0].parent = Some("later".to_string());
        spec.nodes[0].repeat = Some(SculptRepeat {
            count: 33,
            offset: [1.0, 0.0, 0.0],
        });
        let validated = validate_sculpt_spec(spec);
        assert!(validated
            .conflicts
            .iter()
            .any(|item| item.code == "invalid_parent"));
        assert!(validated
            .conflicts
            .iter()
            .any(|item| item.code == "repeat_limit"));
    }
}
