/*
 * tessera_crypt_ocf.h — the kernel's tessera/crypt.h backend over OCF
 * (tessera_crypt_ocf.c). Install at module load; remove at unload.
 */
#ifndef TESSERA_CRYPT_OCF_H_
#define TESSERA_CRYPT_OCF_H_

/* 0 when the backend is installed and its known answers passed; otherwise
 * an errno, no backend, and sealed volumes refuse to mount. */
int  tessera_crypt_ocf_install(void);
void tessera_crypt_ocf_remove(void);

#endif /* TESSERA_CRYPT_OCF_H_ */
