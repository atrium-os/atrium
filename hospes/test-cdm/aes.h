// AES-128, encrypt and decrypt one block (FIPS-197). Test code: plain tables, no
// side-channel care. Used by the Clear Key test CDM only.
#pragma once
#include <stdint.h>

struct Aes128 {
  uint8_t rk[176];
  void set_key(const uint8_t key[16]);
  void encrypt(const uint8_t in[16], uint8_t out[16]) const;
  void decrypt(const uint8_t in[16], uint8_t out[16]) const;
};
