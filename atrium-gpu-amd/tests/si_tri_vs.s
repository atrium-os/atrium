// NGG VS, wave32, GFX12. s[0:1] ring offsets, s2 gs_tg_info, s3 merged_wave_info,
// s[8:9] user data: debug buffer VA. v0 packed prim, v1 prim id, v2 -, v3 vertex id.
  // NGG waves start with only some lanes in EXEC: set it all, as ACO
  // does for every NEXT_GEN_GEOMETRY shader (aco_insert_exec_mask.cpp);
  // merged_wave_info then says which lanes have a vertex or a primitive.
  s_mov_b32 exec_lo, -1
  v_mbcnt_lo_u32_b32 v5, -1, 0
  // debug record per lane: 16 dwords at s[8:9] + lane*64
  v_lshlrev_b32 v12, 6, v5
  v_mov_b32 v13, s0
  v_mov_b32 v14, s1
  v_mov_b32 v15, s2
  v_mov_b32 v16, s3
  global_store_b32 v12, v13, s[8:9] offset:0
  global_store_b32 v12, v14, s[8:9] offset:4
  global_store_b32 v12, v15, s[8:9] offset:8
  global_store_b32 v12, v16, s[8:9] offset:12
  global_store_b32 v12, v0, s[8:9] offset:16
  global_store_b32 v12, v1, s[8:9] offset:20
  global_store_b32 v12, v2, s[8:9] offset:24
  global_store_b32 v12, v3, s[8:9] offset:28
  global_store_b32 v12, v4, s[8:9] offset:32
  v_mov_b32 v13, s4
  v_mov_b32 v14, s5
  v_mov_b32 v15, s6
  v_mov_b32 v16, s7
  global_store_b32 v12, v13, s[8:9] offset:36
  global_store_b32 v12, v14, s[8:9] offset:40
  global_store_b32 v12, v15, s[8:9] offset:44
  global_store_b32 v12, v16, s[8:9] offset:48
  v_mov_b32 v13, 0x7b7b0000
  v_or_b32 v13, v13, v5
  global_store_b32 v12, v13, s[8:9] offset:60
  // wave 0 of the group asks for export space
  s_bfe_u32 s10, s3, 0x40018
  s_cmp_lg_u32 s10, 0
  s_cbranch_scc1 skip
  s_bfe_u32 s11, s2, 0x9000c
  s_bfe_u32 s12, s2, 0x90016
  s_lshl_b32 s12, s12, 12
  s_or_b32 m0, s11, s12
  s_sendmsg sendmsg(MSG_GS_ALLOC_REQ)
skip:
  s_mov_b32 s13, exec_lo
  s_bfe_u32 s14, s3, 0x80008
  v_cmp_gt_u32 vcc_lo, s14, v5
  s_and_b32 exec_lo, s13, vcc_lo
  s_cbranch_execz noprim
  export prim v0, off, off, off done
noprim:
  s_and_b32 s15, s3, 0xff
  v_cmp_gt_u32 vcc_lo, s15, v5
  s_and_b32 exec_lo, s13, vcc_lo
  s_cbranch_execz end
  v_mov_b32 v6, -0.5
  v_mov_b32 v7, -0.5
  v_mov_b32 v8, 0x3e800000
  v_mov_b32 v9, 0x3f400000
  v_cmp_eq_u32 vcc_lo, 1, v3
  v_cndmask_b32_e64 v6, v6, 0.5, vcc_lo
  v_cmp_eq_u32 vcc_lo, 2, v3
  v_cndmask_b32 v6, v6, v8, vcc_lo
  v_cndmask_b32 v7, v7, v9, vcc_lo
  v_mov_b32 v10, 0
  v_mov_b32 v11, 1.0
  export pos0 v6, v7, v10, v11 done
end:
  s_endpgm
