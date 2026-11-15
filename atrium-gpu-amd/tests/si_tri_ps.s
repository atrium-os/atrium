// PS, wave32: count its invocations (atomic add at user SGPRs s[0:1]),
// then export (1, 1, 0, 1) to MRT0.
  v_mov_b32 v4, 1
  v_mov_b32 v5, 0
  global_atomic_add_u32 v5, v4, s[0:1]
  v_mov_b32 v0, 1.0
  v_mov_b32 v1, 1.0
  v_mov_b32 v2, 0
  v_mov_b32 v3, 1.0
  export mrt0 v0, v1, v2, v3 done
  s_endpgm
