# AMD RDNA4 machine-readable ISA

`amdgpu_isa_rdna4.xml.xz` is `amdgpu_isa_rdna4.xml` from AMD's machine-readable
ISA package (`AMD_GPU_MR_ISA_XML_2026_08_06.zip`, downloaded from
https://gpuopen.com/machine-readable-isa/ on 2026-11-03), compressed with xz.
The XML's own header states: "Copyright (c) 2026 Advanced Micro Devices, Inc.,
or its affiliates", license MIT, release date 2026-02-20, schema 1.1.1.

To update: download the new package, replace this file, and run
`python3 -I gen/gen_tables.py spec/amdgpu_isa_rdna4.xml.xz > src/tables.rs`.
