/*
 * rspike.c -- the only C in the Rust spike module: the module declaration.
 * DEV_MODULE is a linker-set macro; rspike_modevent is defined in rspike.rs
 * by define_module!.
 *
 * SPDX-License-Identifier: BSD-2-Clause
 */
#include <sys/param.h>
#include <sys/conf.h>
#include <sys/kernel.h>
#include <sys/module.h>

int rspike_modevent(module_t mod, int type, void *data);

DEV_MODULE(rspike, rspike_modevent, NULL);
MODULE_VERSION(rspike, 1);
