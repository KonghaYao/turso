#ifndef TURSO_TEST_H
#define TURSO_TEST_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

uint32_t turso_test_pending_byte_get(void);
void turso_test_pending_byte_set(uint32_t value);

#ifdef __cplusplus
}
#endif

#endif
