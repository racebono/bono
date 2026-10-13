#pragma once

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
#include <optional>
#endif

typedef struct lambiase_string {
    uint8_t* data;
    size_t len;
    size_t cap;
} lambiase_string_s;

// A value a simulator may not provide, stored as NULL: `value` is only
// meaningful when `present` is true. Named after the single-token types the
// column lists use (f64, i64, boolean, string), so lists can declare fields with
// `lambiase_opt_##type##_s`.
typedef struct lambiase_opt_f64 {
    bool present;
    double value;

#ifdef __cplusplus
    lambiase_opt_f64() noexcept = default;
    lambiase_opt_f64(std::optional<double> x) noexcept :
        present(x.has_value()), value(x.has_value() ? x.value() : 0.0F) {}
#endif
} lambiase_opt_f64_s;

typedef struct lambiase_opt_i64 {
    bool present;
    int64_t value;

#ifdef __cplusplus
    lambiase_opt_i64() noexcept = default;
    lambiase_opt_i64(std::optional<int64_t> x) noexcept :
        present(x.has_value()), value(x.has_value() ? x.value() : 0) {}
#endif
} lambiase_opt_i64_s;

typedef struct lambiase_opt_boolean {
    bool present;
    bool value;

#ifdef __cplusplus
    lambiase_opt_boolean() noexcept = default;
    lambiase_opt_boolean(std::optional<bool> x) noexcept :
        present(x.has_value()), value(x.has_value() ? x.value() : false) {}
#endif
} lambiase_opt_boolean_s;

typedef struct lambiase_opt_string {
    bool present;
    lambiase_string_s value;
} lambiase_opt_string_s;
