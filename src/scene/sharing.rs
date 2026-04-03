use crate::command::protocol::{Hash256, NULL_HASH};
use crate::cas::store::CasStore;

const NODE_LIST_SIZE: usize = 4096;
const NODE_LIST_HEADER: usize = 36;
const HASH_SIZE: usize = 32;
const MAX_LIST_ENTRIES: usize = 126;

fn read_hash(data: &[u8], offset: usize) -> Hash256 {
    let mut h = [0u8; 32];
    h.copy_from_slice(&data[offset..offset + 32]);
    h
}

fn write_hash(data: &mut [u8], offset: usize, hash: &Hash256) {
    data[offset..offset + 32].copy_from_slice(hash);
}

fn read_u32_le(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([data[offset], data[offset+1], data[offset+2], data[offset+3]])
}

fn write_u32_le(data: &mut [u8], offset: usize, val: u32) {
    data[offset..offset+4].copy_from_slice(&val.to_le_bytes());
}

fn write_f32_le(data: &mut [u8], offset: usize, val: f32) {
    data[offset..offset+4].copy_from_slice(&val.to_le_bytes());
}

// ── Node serialization ──────────────────────────────────────────

pub fn serialize_scene_root(
    flags: u8,
    frame_number: u32,
    node_count: u32,
    ambient_rgba: u32,
    child_list: &Hash256,
    camera: &Hash256,
    environment: &Hash256,
) -> [u8; 128] {
    let mut buf = [0u8; 128];
    buf[0] = 0x01;
    buf[1] = flags;
    write_u32_le(&mut buf, 2, frame_number);
    write_u32_le(&mut buf, 6, node_count);
    write_u32_le(&mut buf, 10, ambient_rgba);
    write_hash(&mut buf, 32, child_list);
    write_hash(&mut buf, 64, camera);
    write_hash(&mut buf, 96, environment);
    buf
}

pub fn serialize_scene_node(
    flags: u8,
    layer_mask: u16,
    bound_radius: f32,
    transform: &Hash256,
    renderable: &Hash256,
    children: &Hash256,
) -> [u8; 128] {
    let mut buf = [0u8; 128];
    buf[0] = 0x02;
    buf[1] = flags;
    buf[2..4].copy_from_slice(&layer_mask.to_le_bytes());
    write_f32_le(&mut buf, 8, bound_radius);
    write_hash(&mut buf, 32, transform);
    write_hash(&mut buf, 64, renderable);
    write_hash(&mut buf, 96, children);
    buf
}

pub fn serialize_transform(flags: u8, scale_hint: f32, matrix: &[f32; 16]) -> [u8; 128] {
    let mut buf = [0u8; 128];
    buf[0] = 0x03;
    buf[1] = flags;
    write_f32_le(&mut buf, 4, scale_hint);
    for i in 0..16 {
        write_f32_le(&mut buf, 64 + i * 4, matrix[i]);
    }
    buf
}

// ── NodeList operations ─────────────────────────────────────────

fn parse_node_list(data: &[u8]) -> (u8, Hash256, Vec<Hash256>) {
    let count = data[1];
    let next = read_hash(data, 4);
    let mut entries = Vec::with_capacity(count as usize);
    for i in 0..count as usize {
        let offset = NODE_LIST_HEADER + i * HASH_SIZE;
        if offset + HASH_SIZE > data.len() { break; }
        entries.push(read_hash(data, offset));
    }
    (count, next, entries)
}

fn serialize_node_list(entries: &[Hash256], next: &Hash256) -> Vec<u8> {
    let mut buf = vec![0u8; NODE_LIST_SIZE];
    buf[0] = 0x10;
    buf[1] = entries.len() as u8;
    write_hash(&mut buf, 4, next);
    for (i, hash) in entries.iter().enumerate() {
        write_hash(&mut buf, NODE_LIST_HEADER + i * HASH_SIZE, hash);
    }
    buf
}

// ── Structural sharing operations ───────────────────────────────

pub fn add_to_node_list(
    cas: &mut CasStore,
    list_hash: &Hash256,
    node_hash: &Hash256,
) -> Hash256 {
    if *list_hash == NULL_HASH {
        let list = serialize_node_list(&[*node_hash], &NULL_HASH);
        return cas.store(&list);
    }

    let list_data = match cas.load(list_hash) {
        Some(d) => d.to_vec(),
        None => {
            let list = serialize_node_list(&[*node_hash], &NULL_HASH);
            return cas.store(&list);
        }
    };

    let (_, next, mut entries) = parse_node_list(&list_data);

    if entries.len() < MAX_LIST_ENTRIES {
        entries.push(*node_hash);
        let new_list = serialize_node_list(&entries, &next);
        cas.store(&new_list)
    } else {
        // current list full — create new list, chain via next
        let new_list = serialize_node_list(&[*node_hash], list_hash);
        cas.store(&new_list)
    }
}

pub fn remove_from_node_list(
    cas: &mut CasStore,
    list_hash: &Hash256,
    node_hash: &Hash256,
) -> Hash256 {
    if *list_hash == NULL_HASH { return NULL_HASH; }

    let list_data = match cas.load(list_hash) {
        Some(d) => d.to_vec(),
        None => return NULL_HASH,
    };

    let (_, next, entries) = parse_node_list(&list_data);
    let new_entries: Vec<Hash256> = entries.into_iter()
        .filter(|h| h != node_hash)
        .collect();

    if new_entries.is_empty() && next == NULL_HASH {
        return NULL_HASH;
    }

    let new_list = serialize_node_list(&new_entries, &next);
    cas.store(&new_list)
}

pub fn replace_in_node_list(
    cas: &mut CasStore,
    list_hash: &Hash256,
    old_hash: &Hash256,
    new_hash: &Hash256,
) -> Option<Hash256> {
    if *list_hash == NULL_HASH { return None; }

    let list_data = match cas.load(list_hash) {
        Some(d) => d.to_vec(),
        None => return None,
    };

    let (_, next, entries) = parse_node_list(&list_data);

    let found = entries.iter().any(|h| h == old_hash);
    if found {
        let new_entries: Vec<Hash256> = entries.iter()
            .map(|h| if h == old_hash { *new_hash } else { *h })
            .collect();
        let new_list = serialize_node_list(&new_entries, &next);
        Some(cas.store(&new_list))
    } else if next != NULL_HASH {
        // recurse into chained list
        let new_next = replace_in_node_list(cas, &next, old_hash, new_hash)?;
        let new_list = serialize_node_list(&entries, &new_next);
        Some(cas.store(&new_list))
    } else {
        None
    }
}

// ── Tree-level structural sharing ───────────────────────────────

pub fn update_node_field(
    cas: &mut CasStore,
    root_hash: &Hash256,
    target_node_hash: &Hash256,
    new_node_hash: &Hash256,
) -> Option<Hash256> {
    if *root_hash == NULL_HASH { return None; }

    let root_data = match cas.load(root_hash) {
        Some(d) => d.to_vec(),
        None => return None,
    };

    if root_data[0] != 0x01 { return None; }

    let child_list = read_hash(&root_data, 32);
    let camera = read_hash(&root_data, 64);
    let environment = read_hash(&root_data, 96);

    let new_child_list = replace_in_tree(cas, &child_list, target_node_hash, new_node_hash)?;

    let new_root = serialize_scene_root(
        root_data[1],
        read_u32_le(&root_data, 2),
        read_u32_le(&root_data, 6),
        read_u32_le(&root_data, 10),
        &new_child_list,
        &camera,
        &environment,
    );
    Some(cas.store(&new_root))
}

fn replace_in_tree(
    cas: &mut CasStore,
    list_hash: &Hash256,
    target_hash: &Hash256,
    new_hash: &Hash256,
) -> Option<Hash256> {
    if *list_hash == NULL_HASH { return None; }

    // try direct replacement in this list
    if let Some(new_list) = replace_in_node_list(cas, list_hash, target_hash, new_hash) {
        return Some(new_list);
    }

    // not a direct child — recurse into each child's subtree
    let list_data = match cas.load(list_hash) {
        Some(d) => d.to_vec(),
        None => return None,
    };

    let (_, next, entries) = parse_node_list(&list_data);

    for (i, entry_hash) in entries.iter().enumerate() {
        let node_data = match cas.load(entry_hash) {
            Some(d) => d.to_vec(),
            None => continue,
        };

        if node_data.len() < 128 || node_data[0] != 0x02 { continue; }
        let children = read_hash(&node_data, 96);
        if children == NULL_HASH { continue; }

        if let Some(new_children) = replace_in_tree(cas, &children, target_hash, new_hash) {
            // rebuild this SceneNode with new children hash
            let new_node = serialize_scene_node(
                node_data[1],
                u16::from_le_bytes([node_data[2], node_data[3]]),
                f32::from_le_bytes([node_data[8], node_data[9], node_data[10], node_data[11]]),
                &read_hash(&node_data, 32),
                &read_hash(&node_data, 64),
                &new_children,
            );
            let new_node_hash = cas.store(&new_node);

            // replace this node in the current list
            let mut new_entries = entries.clone();
            new_entries[i] = new_node_hash;
            let new_list = serialize_node_list(&new_entries, &next);
            return Some(cas.store(&new_list));
        }
    }

    // check chained list
    if next != NULL_HASH {
        if let Some(new_next) = replace_in_tree(cas, &next, target_hash, new_hash) {
            let new_list = serialize_node_list(&entries, &new_next);
            return Some(cas.store(&new_list));
        }
    }

    None
}

pub fn add_node_to_parent(
    cas: &mut CasStore,
    root_hash: &Hash256,
    parent_hash: &Hash256,
    node_hash: &Hash256,
) -> Option<Hash256> {
    if *root_hash == NULL_HASH { return None; }

    let root_data = match cas.load(root_hash) {
        Some(d) => d.to_vec(),
        None => return None,
    };

    if root_data[0] != 0x01 { return None; }

    // adding to root's child list
    if *parent_hash == *root_hash {
        let old_child_list = read_hash(&root_data, 32);
        let new_child_list = add_to_node_list(cas, &old_child_list, node_hash);
        let new_root = serialize_scene_root(
            root_data[1],
            read_u32_le(&root_data, 2),
            read_u32_le(&root_data, 6) + 1,
            read_u32_le(&root_data, 10),
            &new_child_list,
            &read_hash(&root_data, 64),
            &read_hash(&root_data, 96),
        );
        return Some(cas.store(&new_root));
    }

    // adding to a SceneNode's child list — find the parent, update it, propagate
    let parent_data = match cas.load(parent_hash) {
        Some(d) => d.to_vec(),
        None => return None,
    };

    if parent_data.len() < 128 || parent_data[0] != 0x02 { return None; }

    let old_children = read_hash(&parent_data, 96);
    let new_children = add_to_node_list(cas, &old_children, node_hash);

    let new_parent = serialize_scene_node(
        parent_data[1],
        u16::from_le_bytes([parent_data[2], parent_data[3]]),
        f32::from_le_bytes([parent_data[8], parent_data[9], parent_data[10], parent_data[11]]),
        &read_hash(&parent_data, 32),
        &read_hash(&parent_data, 64),
        &new_children,
    );
    let new_parent_hash = cas.store(&new_parent);

    update_node_field(cas, root_hash, parent_hash, &new_parent_hash)
}

pub fn remove_node_from_parent(
    cas: &mut CasStore,
    root_hash: &Hash256,
    parent_hash: &Hash256,
    node_hash: &Hash256,
) -> Option<Hash256> {
    if *root_hash == NULL_HASH { return None; }

    let root_data = match cas.load(root_hash) {
        Some(d) => d.to_vec(),
        None => return None,
    };

    if root_data[0] != 0x01 { return None; }

    if *parent_hash == *root_hash {
        let old_child_list = read_hash(&root_data, 32);
        let new_child_list = remove_from_node_list(cas, &old_child_list, node_hash);
        let count = read_u32_le(&root_data, 6).saturating_sub(1);
        let new_root = serialize_scene_root(
            root_data[1],
            read_u32_le(&root_data, 2),
            count,
            read_u32_le(&root_data, 10),
            &new_child_list,
            &read_hash(&root_data, 64),
            &read_hash(&root_data, 96),
        );
        return Some(cas.store(&new_root));
    }

    let parent_data = match cas.load(parent_hash) {
        Some(d) => d.to_vec(),
        None => return None,
    };

    if parent_data.len() < 128 || parent_data[0] != 0x02 { return None; }

    let old_children = read_hash(&parent_data, 96);
    let new_children = remove_from_node_list(cas, &old_children, node_hash);

    let new_parent = serialize_scene_node(
        parent_data[1],
        u16::from_le_bytes([parent_data[2], parent_data[3]]),
        f32::from_le_bytes([parent_data[8], parent_data[9], parent_data[10], parent_data[11]]),
        &read_hash(&parent_data, 32),
        &read_hash(&parent_data, 64),
        &new_children,
    );
    let new_parent_hash = cas.store(&new_parent);

    update_node_field(cas, root_hash, parent_hash, &new_parent_hash)
}

pub fn update_scene_node_transform(
    cas: &mut CasStore,
    root_hash: &Hash256,
    node_hash: &Hash256,
    new_xform_hash: &Hash256,
) -> Option<Hash256> {
    let node_data = match cas.load(node_hash) {
        Some(d) => d.to_vec(),
        None => return None,
    };

    if node_data.len() < 128 || node_data[0] != 0x02 { return None; }

    let new_node = serialize_scene_node(
        node_data[1],
        u16::from_le_bytes([node_data[2], node_data[3]]),
        f32::from_le_bytes([node_data[8], node_data[9], node_data[10], node_data[11]]),
        new_xform_hash,
        &read_hash(&node_data, 64),
        &read_hash(&node_data, 96),
    );
    let new_node_hash = cas.store(&new_node);

    if new_node_hash == *node_hash {
        return Some(*root_hash); // dedup: same transform as before
    }

    update_node_field(cas, root_hash, node_hash, &new_node_hash)
}

pub fn update_scene_node_material(
    cas: &mut CasStore,
    root_hash: &Hash256,
    node_hash: &Hash256,
    new_material_hash: &Hash256,
) -> Option<Hash256> {
    let node_data = match cas.load(node_hash) {
        Some(d) => d.to_vec(),
        None => return None,
    };

    if node_data.len() < 128 || node_data[0] != 0x02 { return None; }

    // node's renderable needs updating — load it, swap material
    let renderable_hash = read_hash(&node_data, 64);
    if renderable_hash == NULL_HASH { return None; }

    let rend_data = match cas.load(&renderable_hash) {
        Some(d) => d.to_vec(),
        None => return None,
    };

    if rend_data.len() < 128 || rend_data[0] != 0x04 { return None; }

    let mut new_rend = [0u8; 128];
    new_rend.copy_from_slice(&rend_data);
    write_hash(&mut new_rend, 64, new_material_hash);
    let new_rend_hash = cas.store(&new_rend);

    let new_node = serialize_scene_node(
        node_data[1],
        u16::from_le_bytes([node_data[2], node_data[3]]),
        f32::from_le_bytes([node_data[8], node_data[9], node_data[10], node_data[11]]),
        &read_hash(&node_data, 32),
        &new_rend_hash,
        &read_hash(&node_data, 96),
    );
    let new_node_hash = cas.store(&new_node);

    update_node_field(cas, root_hash, node_hash, &new_node_hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::graph::SceneGraph;

    #[test]
    fn test_add_remove_structural_sharing() {
        let mut cas = CasStore::new();

        // create an empty root
        let empty_root = serialize_scene_root(0, 0, 0, 0, &NULL_HASH, &NULL_HASH, &NULL_HASH);
        let root_hash = cas.store(&empty_root);

        // create a scene node
        let xform_buf = serialize_transform(0x01, 1.0, &[
            1.0, 0.0, 0.0, 0.0,
            0.0, 1.0, 0.0, 0.0,
            0.0, 0.0, 1.0, 0.0,
            0.0, 0.0, 0.0, 1.0,
        ]);
        let xform_hash = cas.store(&xform_buf);
        let node_buf = serialize_scene_node(0x01, 0, 1.0, &xform_hash, &NULL_HASH, &NULL_HASH);
        let node_hash = cas.store(&node_buf);

        // add node to root
        let root2 = add_node_to_parent(&mut cas, &root_hash, &root_hash, &node_hash).unwrap();
        assert_ne!(root2, root_hash); // new root created

        // old root still in CAS
        assert!(cas.exists(&root_hash));

        // remove node from root
        let root3 = remove_node_from_parent(&mut cas, &root2, &root2, &node_hash).unwrap();
        assert_ne!(root3, root2);
    }

    #[test]
    fn test_update_transform_structural_sharing() {
        let mut cas = CasStore::new();

        // build: root → list → [nodeA]
        let xform = serialize_transform(0x01, 1.0, &[
            1.0, 0.0, 0.0, 0.0,
            0.0, 1.0, 0.0, 0.0,
            0.0, 0.0, 1.0, 0.0,
            0.0, 0.0, 0.0, 1.0,
        ]);
        let xform_hash = cas.store(&xform);
        let node = serialize_scene_node(0x01, 0, 1.0, &xform_hash, &NULL_HASH, &NULL_HASH);
        let node_hash = cas.store(&node);

        let empty_root = serialize_scene_root(0, 0, 0, 0, &NULL_HASH, &NULL_HASH, &NULL_HASH);
        let root_hash = cas.store(&empty_root);
        let root_hash = add_node_to_parent(&mut cas, &root_hash, &root_hash, &node_hash).unwrap();

        // update transform
        let new_xform = serialize_transform(0x02, 1.0, &[
            1.0, 0.0, 0.0, 0.0,
            0.0, 1.0, 0.0, 0.0,
            0.0, 0.0, 1.0, 0.0,
            5.0, 0.0, 0.0, 1.0, // translated X by 5
        ]);
        let new_xform_hash = cas.store(&new_xform);

        let root3 = update_scene_node_transform(&mut cas, &root_hash, &node_hash, &new_xform_hash)
            .unwrap();

        assert_ne!(root3, root_hash);

        // old root and old node still exist (not GC'd yet)
        assert!(cas.exists(&root_hash));
        assert!(cas.exists(&node_hash));
    }

    #[test]
    fn test_dedup_on_revert() {
        let mut cas = CasStore::new();

        let xform_a = serialize_transform(0x01, 1.0, &[
            1.0, 0.0, 0.0, 0.0,  0.0, 1.0, 0.0, 0.0,
            0.0, 0.0, 1.0, 0.0,  0.0, 0.0, 0.0, 1.0,
        ]);
        let xform_a_hash = cas.store(&xform_a);

        let node = serialize_scene_node(0x01, 0, 1.0, &xform_a_hash, &NULL_HASH, &NULL_HASH);
        let node_hash = cas.store(&node);

        let empty_root = serialize_scene_root(0, 0, 0, 0, &NULL_HASH, &NULL_HASH, &NULL_HASH);
        let root_hash = cas.store(&empty_root);
        let root1 = add_node_to_parent(&mut cas, &root_hash, &root_hash, &node_hash).unwrap();

        // move node
        let xform_b = serialize_transform(0x02, 1.0, &[
            1.0, 0.0, 0.0, 0.0,  0.0, 1.0, 0.0, 0.0,
            0.0, 0.0, 1.0, 0.0,  5.0, 0.0, 0.0, 1.0,
        ]);
        let xform_b_hash = cas.store(&xform_b);
        let root2 = update_scene_node_transform(&mut cas, &root1, &node_hash, &xform_b_hash)
            .unwrap();
        assert_ne!(root2, root1);

        // find the new node hash (node with xform_b)
        // the original node_hash had xform_a, now there's a new node with xform_b
        let new_node = serialize_scene_node(0x01, 0, 1.0, &xform_b_hash, &NULL_HASH, &NULL_HASH);
        let new_node_hash = CasStore::hash(&new_node);

        // revert: set transform back to xform_a
        let root3 = update_scene_node_transform(&mut cas, &root2, &new_node_hash, &xform_a_hash)
            .unwrap();

        // CAS dedup: reverted node has same content as original → same hash
        // so root3 should equal root1 (full dedup cascade)
        assert_eq!(root3, root1);
    }

    #[test]
    fn test_full_scene_traversal() {
        // mirrors exactly what test_scene.rs builds
        let mut cas = CasStore::new();

        // identity transform
        let xform = serialize_transform(0x00, 1.0, &[
            1.0, 0.0, 0.0, 0.0,
            0.0, 1.0, 0.0, 0.0,
            0.0, 0.0, 1.0, 0.0,
            0.0, 0.0, 0.0, 1.0,
        ]);
        let xform_hash = cas.store(&xform);

        // triangle vertices (3 × float3 = 36 bytes)
        let verts: Vec<u8> = [0.0f32, 0.5, 0.0, -0.5, -0.5, 0.0, 0.5, -0.5, 0.0]
            .iter().flat_map(|f| f.to_le_bytes()).collect();
        let vert_hash = cas.store(&verts);

        // mesh header (128 bytes) — same as test_scene's make_mesh
        let mut mesh = vec![0u8; 128];
        mesh[0] = 0x08; // NODE_MESH_HEADER
        mesh[2..6].copy_from_slice(&3u32.to_le_bytes()); // vertex_count
        mesh[10..12].copy_from_slice(&12u16.to_le_bytes()); // vertex_stride
        mesh[32..64].copy_from_slice(&vert_hash); // vertex_data hash
        let mesh_hash = cas.store(&mesh);

        // material (128 bytes)
        let mut material = vec![0u8; 128];
        material[0] = 0x05;
        material[2..6].copy_from_slice(&0xFF0000FFu32.to_le_bytes());
        let mat_hash = cas.store(&material);

        // renderable (128 bytes)
        let mut renderable = vec![0u8; 128];
        renderable[0] = 0x04;
        renderable[32..64].copy_from_slice(&mesh_hash);
        renderable[64..96].copy_from_slice(&mat_hash);
        let rend_hash = cas.store(&renderable);

        // scene node (128 bytes, flags=0x01 VISIBLE)
        let node = serialize_scene_node(0x01, 0, 1.0, &xform_hash, &rend_hash, &NULL_HASH);
        let node_hash = cas.store(&node);

        // node list (4KB)
        let mut list = vec![0u8; 4096];
        list[0] = 0x10;
        list[1] = 1; // count
        list[36..68].copy_from_slice(&node_hash);
        let list_hash = cas.store(&list);

        // camera
        let cam_xform = serialize_transform(0x00, 1.0, &[
            1.0, 0.0, 0.0, 0.0,
            0.0, 1.0, 0.0, 0.0,
            0.0, 0.0, 1.0, 0.0,
            0.0, 0.0, 3.0, 1.0,
        ]);
        let cam_xform_hash = cas.store(&cam_xform);
        let mut camera = vec![0u8; 128];
        camera[0] = 0x06;
        camera[4..8].copy_from_slice(&std::f32::consts::FRAC_PI_4.to_le_bytes());
        camera[8..12].copy_from_slice(&(1024.0f32 / 768.0).to_le_bytes());
        camera[12..16].copy_from_slice(&0.1f32.to_le_bytes());
        camera[16..20].copy_from_slice(&100.0f32.to_le_bytes());
        camera[32..64].copy_from_slice(&cam_xform_hash);
        let cam_hash = cas.store(&camera);

        // scene root
        let root = serialize_scene_root(0, 0, 1, 0, &list_hash, &cam_hash, &NULL_HASH);
        let root_hash = cas.store(&root);

        // now traverse
        let mut scene = SceneGraph::new();
        scene.set_root(root_hash);
        scene.traverse(&mut cas);

        eprintln!("render_list len = {}", scene.render_list().len());
        eprintln!("root_hash = {:02x}{:02x}", root_hash[0], root_hash[1]);
        eprintln!("list_hash = {:02x}{:02x}", list_hash[0], list_hash[1]);
        eprintln!("node_hash = {:02x}{:02x}", node_hash[0], node_hash[1]);
        eprintln!("rend_hash = {:02x}{:02x}", rend_hash[0], rend_hash[1]);
        eprintln!("mesh_hash = {:02x}{:02x}", mesh_hash[0], mesh_hash[1]);

        assert_eq!(scene.render_list().len(), 1, "should find 1 renderable node");
        assert_eq!(scene.render_list()[0].mesh, mesh_hash);
        assert_eq!(scene.render_list()[0].material, mat_hash);
    }

    #[test]
    fn test_upload_protocol_hash_match() {
        // simulate the upload protocol: split data into chunks, reassemble, check hash matches
        let mut cas = CasStore::new();

        let xform = serialize_transform(0x00, 1.0, &[
            1.0, 0.0, 0.0, 0.0,
            0.0, 1.0, 0.0, 0.0,
            0.0, 0.0, 1.0, 0.0,
            0.0, 0.0, 0.0, 1.0,
        ]);
        assert_eq!(xform.len(), 128);

        // client computes hash
        let client_hash = CasStore::hash(&xform);

        // simulate upload: BEGIN with first 112 bytes, DATA with remaining 16
        let total_size = xform.len();
        let first_chunk = total_size.min(112);
        cas.begin_upload(1, total_size, &xform[..first_chunk]);

        let mut pos = first_chunk;
        while pos < total_size {
            let chunk = (total_size - pos).min(116);
            cas.append_upload(1, &xform[pos..pos+chunk]);
            pos += chunk;
        }

        let server_hash = cas.finish_upload(1).unwrap();

        eprintln!("client hash: {:02x}{:02x}...", client_hash[0], client_hash[1]);
        eprintln!("server hash: {:02x}{:02x}...", server_hash[0], server_hash[1]);
        assert_eq!(client_hash, server_hash, "upload protocol must preserve hash");

        // also test 4KB blob (NodeList)
        let mut list = vec![0u8; 4096];
        list[0] = 0x10;
        list[1] = 1;
        let client_list_hash = CasStore::hash(&list);

        let first = list.len().min(112);
        cas.begin_upload(2, list.len(), &list[..first]);
        let mut pos = first;
        while pos < list.len() {
            let chunk = (list.len() - pos).min(116);
            cas.append_upload(2, &list[pos..pos+chunk]);
            pos += chunk;
        }
        let server_list_hash = cas.finish_upload(2).unwrap();
        eprintln!("list client: {:02x}{:02x}...", client_list_hash[0], client_list_hash[1]);
        eprintln!("list server: {:02x}{:02x}...", server_list_hash[0], server_list_hash[1]);
        assert_eq!(client_list_hash, server_list_hash, "4KB upload must preserve hash");
    }
}
