use super::*;
use libcompositing_rust::display_list::commands::ContextRef;
use libcompositing_rust::visual_context::{
    ClipMode, ClipNodeData, VISUAL_VIEWPORT_NODE_INDEX as ROOT, WorldTransforms,
};

pub(super) fn hidden(tree: &VisualContextTree, mut id: EffectNodeIndex) -> Result<bool, String> {
    for _ in 0..=tree.effect_nodes.len() {
        if id.is_none() {
            return Ok(false);
        }
        let e = tree
            .effect_nodes
            .get(id.0 as usize)
            .ok_or("invalid paint effect")?;
        if let EffectNodeData::Effects(f) = &e.data
            && f.opacity == 0.
            && f.blend_mode == CompositingAndBlendingOperator::Normal
        {
            return Ok(true);
        }
        id = e.parent;
    }
    Err("cyclic paint effect".into())
}

fn matrix(world: &WorldTransforms, space: SpatialNodeIndex) -> Result<[f32; 6], String> {
    let m = world
        .draw_matrix_if_known(space)
        .ok_or("unknown mask frame space")?
        .elements;
    affine_on_plane(m).ok_or_else(|| format!("nonplanar mask frame {space:?}: {m:?}"))
}

fn affine_on_plane(m: [[f32; 4]; 4]) -> Option<[f32; 6]> {
    // Local ink lies at z=0. Scaling the perpendicular axis leaves that plane fixed,
    // including the roundoff a sampled rotation introduces in its z coefficient.
    let [
        [a, c, 0., x],
        [b, d, 0., y],
        [0., 0., z_scale, 0.],
        [0., 0., 0., 1.],
    ] = m
    else {
        return None;
    };
    [a, b, c, d, x, y, z_scale]
        .into_iter()
        .all(f32::is_finite)
        .then_some([a, b, c, d, x, y])
}
pub(super) fn to_viewport(
    tree: &VisualContextTree,
    world: &WorldTransforms,
    leaf: &mut PaintLeaf,
) -> Result<(), String> {
    let mut clip = leaf.context.clip;
    let mut bounds = None;
    let mut intersect = |rect: FloatRect, space| -> Result<(), String> {
        let [a, 0., 0., d, x, y] = matrix(world, space)? else {
            return Err("rotated mask frame clip".into());
        };
        if a <= 0. || d <= 0. {
            return Err("inverted mask frame clip".into());
        }
        intersect_clip(
            &mut bounds,
            FloatRect::new(
                rect.x * a + x,
                rect.y * d + y,
                rect.width * a,
                rect.height * d,
            ),
        )?;
        Ok(())
    };
    if let Some(r) = leaf.inline_clip {
        intersect(r, leaf.context.spatial)?;
    }
    for _ in 0..=tree.clip_nodes.len() {
        if clip.is_none() {
            break;
        }
        let c = tree
            .clip_nodes
            .get(clip.0 as usize)
            .ok_or("missing mask frame clip")?;
        let ClipNodeData::Rect(r) = &c.data else {
            return Err("path clip on masked frame".into());
        };
        if r.mode != ClipMode::Intersect || r.corner_radii != Default::default() {
            return Err("nonrectangular mask frame clip".into());
        }
        intersect(r.rect, c.spatial)?;
        clip = c.parent;
    }
    if !clip.is_none() {
        return Err("cyclic mask frame clip".into());
    }
    let m = matrix(world, leaf.context.spatial)?;
    if m != [1., 0., 0., 1., 0., 0.] {
        match &mut leaf.node {
            Node::Glyphs { origin, .. } if m[..4] == [1., 0., 0., 1.] => {
                origin.x += m[4];
                origin.y += m[5];
            }
            Node::Fill(_) | Node::Rect { radius: 0., .. } => {
                leaf.node = place(leaf.node.clone(), m)?
            }
            _ => return Err("unsupported transformed mask frame primitive".into()),
        }
    }
    leaf.inline_clip = bounds;
    leaf.context = ContextRef {
        effect: leaf.context.effect,
        ..ContextRef::spatial_only(ROOT)
    };
    Ok(())
}

#[cfg(test)]
#[test]
fn spinner_keeps_its_affine_map_but_perspective_is_rejected() {
    let mut m = [
        [-0.25, -0.968, 0., 648.],
        [0.968, -0.25, 0., -146.],
        [0., 0., 0.99999994, 0.],
        [0., 0., 0., 1.],
    ];
    assert_eq!(
        affine_on_plane(m),
        Some([-0.25, 0.968, -0.968, -0.25, 648., -146.])
    );
    m[3][0] = 0.1;
    assert_eq!(affine_on_plane(m), None);
}
