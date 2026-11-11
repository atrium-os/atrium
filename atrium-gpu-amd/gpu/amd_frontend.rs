/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * amd_frontend.rs — crate root of atrium_gpu_amd.ko's Rust part: the
 * transport-neutral front-end (client node ioctls, VM / BO / syncobj fd
 * objects). The hardware backend under it stays C. See ../rs/README.md.
 *
 * Built by freebsd-kpi-rs's kmod.rust.mk (RUST_SRCS in this directory's
 * Makefile); the modules live in ../rs and are named here by path.
 */
#![no_std]

#[path = "../rs/sys.rs"]
pub mod sys;
#[path = "../rs/backend.rs"]
pub mod backend;
#[path = "../rs/syncobj.rs"]
pub mod syncobj;
#[path = "../rs/vm.rs"]
pub mod vm;
#[path = "../rs/bo.rs"]
pub mod bo;
#[path = "../rs/ioctl.rs"]
pub mod ioctl;
#[path = "../rs/doorbell.rs"]
pub mod doorbell;
#[path = "../rs/defer.rs"]
pub mod defer;
