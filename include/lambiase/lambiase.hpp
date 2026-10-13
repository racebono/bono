#pragma once

#include <memory_resource>

#include "lambiase.h"

extern "C" {
void* lambiase_malloc(size_t bytes, size_t alignment);
void lambiase_free(void* p, size_t bytes, size_t alignment);
}

struct lambiase_memory_resouce : public std::pmr::memory_resource {
protected:
    inline void* do_allocate(size_t bytes, size_t align) override {
        return lambiase_malloc(bytes, align);
    }

    inline void do_deallocate(void* p, size_t bytes, size_t align) override {
        lambiase_free(p, bytes, align);
    }

    bool do_is_equal(const memory_resource& That) const noexcept override {
        return true;
    }
};
