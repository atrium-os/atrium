pub type Hash256 = [u8; 32];
pub const NULL_HASH: Hash256 = [0u8; 32];

#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Command {
    pub opcode: u16,
    pub flags: u16,
    pub sequence_id: u32,
    pub payload: [u32; 30], // 120 bytes — fits two 32-byte hashes + metadata
}

#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Completion {
    pub comp_type: u16,
    pub status: u16,
    pub id: u32,
    pub result_hash: Hash256,
    pub _pad: [u32; 22], // pad to 128 bytes total
}

// Command opcodes — matches KarythraGPU SCENE_GRAPH.md exactly

// Resource upload
pub const CMD_UPLOAD_BEGIN: u16 = 0x0001;
pub const CMD_UPLOAD_DATA: u16 = 0x0002;
pub const CMD_UPLOAD_FINISH: u16 = 0x0003;
pub const CMD_UPLOAD_DMA: u16 = 0x0004;

// Scene graph
pub const CMD_SET_ROOT: u16 = 0x0100;
pub const CMD_SET_CAMERA: u16 = 0x0101;
pub const CMD_ADD_NODE: u16 = 0x0102;
pub const CMD_REMOVE_NODE: u16 = 0x0103;
pub const CMD_UPDATE_TRANSFORM: u16 = 0x0104;
pub const CMD_UPDATE_TRANSFORM_INLINE: u16 = 0x0105;
pub const CMD_UPDATE_MATERIAL: u16 = 0x0106;
pub const CMD_ADD_LIGHT: u16 = 0x0107;
pub const CMD_UPDATE_LIGHT: u16 = 0x0108;

// Autonomous tasks
pub const CMD_SPAWN_TASK: u16 = 0x0200;
pub const CMD_SPAWN_TASK_TARGET: u16 = 0x0201;
pub const CMD_STOP_TASK: u16 = 0x0202;

// Control
pub const CMD_RENDER: u16 = 0x0300;
pub const CMD_FENCE: u16 = 0x0301;
pub const CMD_QUERY_HASH: u16 = 0x0302;

// Completion types
pub const COMP_UPLOAD_COMPLETE: u16 = 0x01;
pub const COMP_FENCE: u16 = 0x02;
pub const COMP_QUERY_RESULT: u16 = 0x03;
pub const COMP_ERROR: u16 = 0xFF;

// Status codes
pub const STATUS_OK: u16 = 0x00;
pub const STATUS_CAS_FULL: u16 = 0x01;
pub const STATUS_INVALID_HASH: u16 = 0x02;
pub const STATUS_EXISTS: u16 = 0x03;
pub const STATUS_NOT_FOUND: u16 = 0x04;

// Scene node types — matches KarythraGPU SCENE_GRAPH.md
pub const NODE_SCENE_ROOT: u8 = 0x01;
pub const NODE_SCENE_NODE: u8 = 0x02;
pub const NODE_TRANSFORM: u8 = 0x03;
pub const NODE_RENDERABLE: u8 = 0x04;
pub const NODE_MATERIAL: u8 = 0x05;
pub const NODE_CAMERA: u8 = 0x06;
pub const NODE_LIGHT: u8 = 0x07;
pub const NODE_MESH_HEADER: u8 = 0x08;
pub const NODE_TEXTURE_HEADER: u8 = 0x09;
pub const NODE_AUTONOMOUS_TASK: u8 = 0x0A;
pub const NODE_COLLIDER: u8 = 0x0B;
pub const NODE_SHAPE_DATA: u8 = 0x0C;
pub const NODE_PATH_HEADER: u8 = 0x0D;
pub const NODE_NODE_LIST: u8 = 0x10;
pub const NODE_PHYSICS_CONFIG: u8 = 0x10; // 4KB blob
pub const NODE_PHYSICS_BODY_REG: u8 = 0x11;
pub const NODE_PHYSICS_STATE: u8 = 0x12;
pub const NODE_JOINT: u8 = 0x13;
pub const NODE_CONTACT_LIST: u8 = 0x15;

// Command priority classification
pub enum CommandPriority {
    Immediate, // mid-frame safe: transforms, camera, lights, materials
    Deferred,  // frame boundary: structural changes, uploads, tasks
}

impl Command {
    pub fn payload_bytes(&self) -> &[u8] {
        bytemuck::cast_slice(&self.payload)
    }

    pub fn priority(&self) -> CommandPriority {
        match self.opcode {
            CMD_UPDATE_TRANSFORM | CMD_UPDATE_TRANSFORM_INLINE
            | CMD_SET_CAMERA | CMD_UPDATE_LIGHT | CMD_UPDATE_MATERIAL => {
                CommandPriority::Immediate
            }
            _ => CommandPriority::Deferred,
        }
    }

    pub fn hash_at(&self, offset: usize) -> Hash256 {
        let bytes = self.payload_bytes();
        let start = offset - 8; // payload starts at byte 8 of Command
        let mut h = [0u8; 32];
        h.copy_from_slice(&bytes[start..start + 32]);
        h
    }

    pub fn u32_at(&self, offset: usize) -> u32 {
        let bytes = self.payload_bytes();
        let start = offset - 8;
        u32::from_le_bytes([bytes[start], bytes[start+1], bytes[start+2], bytes[start+3]])
    }

    pub fn f32_at(&self, offset: usize) -> f32 {
        f32::from_le_bytes(self.u32_at(offset).to_le_bytes())
    }
}

impl Completion {
    pub fn upload_complete(upload_id: u32, hash: Hash256) -> Self {
        Self {
            comp_type: COMP_UPLOAD_COMPLETE,
            status: STATUS_OK,
            id: upload_id,
            result_hash: hash,
            _pad: [0; 22],
        }
    }

    pub fn fence(fence_value: u32) -> Self {
        Self {
            comp_type: COMP_FENCE,
            status: STATUS_OK,
            id: fence_value,
            result_hash: [0; 32],
            _pad: [0; 22],
        }
    }

    pub fn query_result(query_id: u32, exists: bool) -> Self {
        Self {
            comp_type: COMP_QUERY_RESULT,
            status: if exists { STATUS_EXISTS } else { STATUS_NOT_FOUND },
            id: query_id,
            result_hash: [0; 32],
            _pad: [0; 22],
        }
    }

    pub fn error(sequence_id: u32, status: u16) -> Self {
        Self {
            comp_type: COMP_ERROR,
            status,
            id: sequence_id,
            result_hash: [0; 32],
            _pad: [0; 22],
        }
    }
}
