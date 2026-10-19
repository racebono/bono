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

// A Rust `String`: only Rust allocates and frees it, through bono_string_new
// and bono_string_drop (implemented by bono-sim). Always valid UTF-8.
typedef struct bono_string {
    uint8_t* data;
    size_t len;
    size_t cap;

#ifdef __cplusplus
    bono_string(std::string_view value) noexcept;
    // Leaves `other` without data: it frees nothing.
    bono_string(bono_string&& other) noexcept;
    bono_string& operator=(bono_string&& other) noexcept;
    ~bono_string() noexcept;
#endif
} bono_string_s;

#ifdef __cplusplus
extern "C" {
#endif

// Writes a new string to `out` with the `len` bytes at `data`, invalid UTF-8
// replaced.
void bono_string_new(bono_string_s* out, const uint8_t* data, size_t len);
// Frees a string made by bono_string_new.
void bono_string_drop(bono_string_s* string);

#ifdef __cplusplus
}

inline bono_string::bono_string(std::string_view value) noexcept {
    bono_string_new(this, reinterpret_cast<const uint8_t*>(value.data()), value.size());
}

inline bono_string::bono_string(bono_string&& other) noexcept :
    data(std::exchange(other.data, nullptr)), len(std::exchange(other.len, 0)),
    cap(std::exchange(other.cap, 0)) {}

inline bono_string& bono_string::operator=(bono_string&& other) noexcept {
    if (this != &other) {
        this->~bono_string();
        new (this) bono_string(std::move(other));
    }
    return *this;
}

inline bono_string::~bono_string() noexcept {
    if (data) bono_string_drop(this);
}
#endif

// A value a simulator may not provide, stored as NULL: `value` is only
// meaningful when `present` is true. Named after the single-token types the
// column lists use (f64, i64, boolean, string), so lists can declare fields with
// `bono_opt_##type##_s`.
typedef struct bono_opt_f64 {
    bool present;
    double value;

#ifdef __cplusplus
    bono_opt_f64() noexcept : present(false), value(0.0) {}
    bono_opt_f64(std::nullopt_t) noexcept : bono_opt_f64() {}
    bono_opt_f64(double x) noexcept : present(true), value(x) {}
    bono_opt_f64(std::optional<double> x) noexcept :
        present(x.has_value()), value(x.has_value() ? x.value() : 0.0) {}
#endif
} bono_opt_f64_s;

typedef struct bono_opt_i64 {
    bool present;
    int64_t value;

#ifdef __cplusplus
    bono_opt_i64() noexcept : present(false), value(0) {}
    bono_opt_i64(std::nullopt_t) noexcept : bono_opt_i64() {}
    bono_opt_i64(int64_t x) noexcept : present(true), value(x) {}
    bono_opt_i64(std::optional<int64_t> x) noexcept :
        present(x.has_value()), value(x.has_value() ? x.value() : 0) {}
#endif
} bono_opt_i64_s;

typedef struct bono_opt_boolean {
    bool present;
    bool value;

#ifdef __cplusplus
    bono_opt_boolean() noexcept : present(false), value(false) {}
    bono_opt_boolean(std::nullopt_t) noexcept : bono_opt_boolean() {}
    bono_opt_boolean(bool x) noexcept : present(true), value(x) {}
    bono_opt_boolean(std::optional<bool> x) noexcept :
        present(x.has_value()), value(x.has_value() ? x.value() : false) {}
#endif
} bono_opt_boolean_s;

typedef struct bono_opt_string {
    bool present;

#ifdef __cplusplus
    // In a union so it's only constructed and destroyed while `present`; the
    // layout is the same as C's.
    union {
        bono_string_s value;
    };

    bono_opt_string() noexcept : present(false) {}
    bono_opt_string(std::nullopt_t) noexcept : bono_opt_string() {}
    bono_opt_string(std::string_view x) noexcept : present(true), value(x) {}
    bono_opt_string(const char* x) noexcept : bono_opt_string(std::string_view(x)) {}

    // Any optional string-like value, e.g. std::optional<std::string>.
    template<class T>
    bono_opt_string(const std::optional<T>& x) noexcept : bono_opt_string() {
        if (x) *this = bono_opt_string(std::string_view(*x));
    }

    // Leaves `other` empty.
    bono_opt_string(bono_opt_string&& other) noexcept : present(other.present) {
        if (present) {
            new (&value) bono_string(std::move(other.value));
            other.value.~bono_string();
            other.present = false;
        }
    }

    bono_opt_string& operator=(bono_opt_string&& other) noexcept {
        if (this != &other) {
            this->~bono_opt_string();
            new (this) bono_opt_string(std::move(other));
        }
        return *this;
    }

    ~bono_opt_string() noexcept {
        if (present) value.~bono_string();
    }
#else
    bono_string_s value;
#endif
} bono_opt_string_s;
