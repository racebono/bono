#pragma once

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

void *lambiase_malloc(size_t bytes, size_t alignment);
void lambiase_free(void *p, size_t bytes, size_t alignment);

#ifdef __cplusplus
}
#endif

typedef struct lambiase_string {
  uint8_t *data;
  size_t len;
} lambiase_string_s;
