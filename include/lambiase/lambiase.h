#pragma once

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
#include <new>
#include <optional>
#include <string_view>
#include <utility>
#endif

// A Rust `String`: only Rust allocates and frees it, through lambiase_string_new
// and lambiase_string_drop (implemented by lambiase-sim). Always valid UTF-8.
typedef struct lambiase_string {
    uint8_t* data;
    size_t len;
    size_t cap;

#ifdef __cplusplus
    lambiase_string(std::string_view value) noexcept;
    // Leaves `other` without data: it frees nothing.
    lambiase_string(lambiase_string&& other) noexcept;
    lambiase_string& operator=(lambiase_string&& other) noexcept;
    ~lambiase_string() noexcept;
#endif
} lambiase_string_s;

#ifdef __cplusplus
extern "C" {
#endif

// Writes a new string to `out` with the `len` bytes at `data`, invalid UTF-8
// replaced.
void lambiase_string_new(lambiase_string_s* out, const uint8_t* data, size_t len);
// Frees a string made by lambiase_string_new.
void lambiase_string_drop(lambiase_string_s* string);

#ifdef __cplusplus
}

inline lambiase_string::lambiase_string(std::string_view value) noexcept {
    lambiase_string_new(this, reinterpret_cast<const uint8_t*>(value.data()), value.size());
}

inline lambiase_string::lambiase_string(lambiase_string&& other) noexcept :
    data(std::exchange(other.data, nullptr)), len(std::exchange(other.len, 0)),
    cap(std::exchange(other.cap, 0)) {}

inline lambiase_string& lambiase_string::operator=(lambiase_string&& other) noexcept {
    if (this != &other) {
        this->~lambiase_string();
        new (this) lambiase_string(std::move(other));
    }
    return *this;
}

inline lambiase_string::~lambiase_string() noexcept {
    if (data) lambiase_string_drop(this);
}
#endif

// A value a simulator may not provide, stored as NULL: `value` is only
// meaningful when `present` is true. Named after the single-token types the
// column lists use (f64, i64, boolean, string), so lists can declare fields with
// `lambiase_opt_##type##_s`.
typedef struct lambiase_opt_f64 {
    bool present;
    double value;

#ifdef __cplusplus
    lambiase_opt_f64() noexcept : present(false), value(0.0) {}
    lambiase_opt_f64(std::nullopt_t) noexcept : lambiase_opt_f64() {}
    lambiase_opt_f64(double x) noexcept : present(true), value(x) {}
    lambiase_opt_f64(std::optional<double> x) noexcept :
        present(x.has_value()), value(x.has_value() ? x.value() : 0.0) {}
#endif
} lambiase_opt_f64_s;

typedef struct lambiase_opt_i64 {
    bool present;
    int64_t value;

#ifdef __cplusplus
    lambiase_opt_i64() noexcept : present(false), value(0) {}
    lambiase_opt_i64(std::nullopt_t) noexcept : lambiase_opt_i64() {}
    lambiase_opt_i64(int64_t x) noexcept : present(true), value(x) {}
    lambiase_opt_i64(std::optional<int64_t> x) noexcept :
        present(x.has_value()), value(x.has_value() ? x.value() : 0) {}
#endif
} lambiase_opt_i64_s;

typedef struct lambiase_opt_boolean {
    bool present;
    bool value;

#ifdef __cplusplus
    lambiase_opt_boolean() noexcept : present(false), value(false) {}
    lambiase_opt_boolean(std::nullopt_t) noexcept : lambiase_opt_boolean() {}
    lambiase_opt_boolean(bool x) noexcept : present(true), value(x) {}
    lambiase_opt_boolean(std::optional<bool> x) noexcept :
        present(x.has_value()), value(x.has_value() ? x.value() : false) {}
#endif
} lambiase_opt_boolean_s;

typedef struct lambiase_opt_string {
    bool present;

#ifdef __cplusplus
    // In a union so it's only constructed and destroyed while `present`; the
    // layout is the same as C's.
    union {
        lambiase_string_s value;
    };

    lambiase_opt_string() noexcept : present(false) {}
    lambiase_opt_string(std::nullopt_t) noexcept : lambiase_opt_string() {}
    lambiase_opt_string(std::string_view x) noexcept : present(true), value(x) {}
    lambiase_opt_string(const char* x) noexcept : lambiase_opt_string(std::string_view(x)) {}

    // Any optional string-like value, e.g. std::optional<std::string>.
    template<class T>
    lambiase_opt_string(const std::optional<T>& x) noexcept : lambiase_opt_string() {
        if (x) *this = lambiase_opt_string(std::string_view(*x));
    }

    // Leaves `other` empty.
    lambiase_opt_string(lambiase_opt_string&& other) noexcept : present(other.present) {
        if (present) {
            new (&value) lambiase_string(std::move(other.value));
            other.value.~lambiase_string();
            other.present = false;
        }
    }

    lambiase_opt_string& operator=(lambiase_opt_string&& other) noexcept {
        if (this != &other) {
            this->~lambiase_opt_string();
            new (this) lambiase_opt_string(std::move(other));
        }
        return *this;
    }

    ~lambiase_opt_string() noexcept {
        if (present) value.~lambiase_string();
    }
#else
    lambiase_string_s value;
#endif
} lambiase_opt_string_s;
