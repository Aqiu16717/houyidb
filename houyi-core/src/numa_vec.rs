//! NUMA 感知向量存储。
//! NUMA-aware vector storage with NUMA page binding via mbind().
//!
//! Linux: 调用 `mbind(MPOL_BIND)` 将物理内存页绑定到指定 NUMA 节点。
//! macOS/other: 仅保留 `numa_node` 元数据，实际绑定不生效。
//!
//! Linux: calls `mbind(MPOL_BIND)` to pin physical pages to a NUMA node.
//! macOS/other: metadata-only, no actual page binding.

use std::alloc::{alloc, dealloc, Layout};

/// 将已分配内存页绑定到指定 NUMA 节点（Linux only）。
/// Bind allocated memory pages to a NUMA node (Linux only).
///
/// 为什么走 syscall 而非 libc::mbind：mbind(2) 的用户态包装在 libnuma（numaif.h）
/// 里，glibc 与 libc crate 都不导出它（Linux 编译路径曾因此整体断裂——本地/CI 均为
/// macOS，此函数从未被编译到，属潜伏 bug）。直调 `syscall(SYS_mbind)` 零新增链接依赖。
#[cfg(target_os = "linux")]
unsafe fn bind_pages_to_numa(ptr: *mut f32, size_bytes: usize, numa_node: usize) {
    // numaif.h 的 stable ABI 常量（内核头 linux/mempolicy.h，值逐字同源）。
    // EN: Stable ABI constants from numaif.h (kernel header linux/mempolicy.h, copied verbatim).
    const MPOL_BIND: libc::c_int = 2;
    const MPOL_MF_STRICT: libc::c_uint = 1;
    const MPOL_MF_MOVE: libc::c_uint = 2;

    let nodemask: libc::c_ulong = 1u64 << (numa_node as u64);
    let ret = libc::syscall(
        libc::SYS_mbind,
        ptr as *mut libc::c_void,
        size_bytes as libc::c_ulong,
        MPOL_BIND,
        &nodemask as *const libc::c_ulong,
        (numa_node + 1) as libc::c_ulong,
        MPOL_MF_STRICT | MPOL_MF_MOVE,
    );
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        tracing::warn!(numa_node, size_bytes, error = %err, "houyi-core numa: mbind() failed");
    }
}

#[cfg(not(target_os = "linux"))]
unsafe fn bind_pages_to_numa(_ptr: *mut f32, _size_bytes: usize, _numa_node: usize) {
    // macOS/其他平台：mbind() 不可用，仅保留元数据。
    // EN: macOS/other: mbind() not available, metadata-only
}

/// NUMA 感知向量存储——分配时自动绑定物理页到指定 NUMA 节点。
/// NUMA-aware vector storage with automatic page binding on allocation.
pub struct NumaVec {
    ptr: *mut f32,
    len: usize,
    capacity: usize,
    numa_node: Option<usize>,
}

impl Default for NumaVec {
    fn default() -> Self {
        Self::new()
    }
}

impl NumaVec {
    pub fn new() -> Self {
        NumaVec {
            ptr: std::ptr::null_mut(),
            len: 0,
            capacity: 0,
            numa_node: None,
        }
    }

    /// 在指定 NUMA 节点上创建预分配存储，调用 mbind() 绑定物理页。
    /// Pre-allocate on a specific NUMA node with mbind() page binding.
    pub fn with_numa_node(numa_node: usize, size_bytes: usize) -> Self {
        let count = (size_bytes / 4).max(64);
        let layout = Layout::from_size_align(count * 4, 64).expect("invalid layout");
        let ptr = unsafe { alloc(layout) } as *mut f32;
        unsafe {
            bind_pages_to_numa(ptr, count * 4, numa_node);
        }
        NumaVec {
            ptr,
            len: 0,
            capacity: count,
            numa_node: Some(numa_node),
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn numa_node(&self) -> Option<usize> {
        self.numa_node
    }

    pub fn extend_from_slice(&mut self, slice: &[f32]) {
        let new_len = self.len + slice.len();
        if new_len > self.capacity {
            let new_cap = (new_len * 2).max(64);
            let layout = Layout::from_size_align(new_cap * 4, 64).expect("invalid layout");
            let new_ptr = unsafe { alloc(layout) } as *mut f32;
            // 扩容后重新绑定到同一 NUMA 节点
            // EN: Rebind to the same NUMA node after growing.
            if let Some(node) = self.numa_node {
                unsafe {
                    bind_pages_to_numa(new_ptr, new_cap * 4, node);
                }
            }
            if !self.ptr.is_null() && self.capacity > 0 {
                unsafe {
                    std::ptr::copy_nonoverlapping(self.ptr, new_ptr, self.len);
                }
                let old_layout = Layout::from_size_align(self.capacity * 4, 64).unwrap();
                unsafe {
                    dealloc(self.ptr as *mut u8, old_layout);
                }
            }
            self.ptr = new_ptr;
            self.capacity = new_cap;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(slice.as_ptr(), self.ptr.add(self.len), slice.len());
        }
        self.len = new_len;
    }

    pub fn as_slice(&self) -> &[f32] {
        if self.ptr.is_null() {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
        }
    }

    pub fn get(&self, range: std::ops::Range<usize>) -> Option<&[f32]> {
        self.as_slice().get(range)
    }
}

impl Drop for NumaVec {
    fn drop(&mut self) {
        if !self.ptr.is_null() && self.capacity > 0 {
            let layout = Layout::from_size_align(self.capacity * 4, 64).unwrap();
            unsafe {
                dealloc(self.ptr as *mut u8, layout);
            }
        }
    }
}

unsafe impl Send for NumaVec {}
unsafe impl Sync for NumaVec {}
