//! 全局分配器（tikv-jemallocator）。
//!
//! Phi 扩展是长驻进程：与宿主握手后进入 PXB 读写循环，整个会话期间持续处理
//! 工具调用与钩子事件。分配热点集中在「大块字符串拼接」与「短生命周期
//! `Vec<&str>` 行索引」，系统默认分配器在长会话下容易累积碎片。
//!
//! 全局分配器静态项声明在本 crate，所有依赖 `phi-ext-common` 的扩展二进制
//! 都会自动使用 jemalloc，无需在各自的 `main.rs` 重复声明（同一二进制内
//! 只能有一个 `#[global_allocator]`）。
//!
//! 在 MSVC 与 Android 上 jemalloc 不可用，此时自动退回系统分配器。

/// jemalloc 是否真正生效。
#[cfg(all(
    feature = "jemalloc",
    not(target_env = "msvc"),
    not(target_os = "android")
))]
pub const JEMALLOC_ACTIVE: bool = true;

/// jemalloc 是否真正生效。
#[cfg(not(all(
    feature = "jemalloc",
    not(target_env = "msvc"),
    not(target_os = "android")
)))]
pub const JEMALLOC_ACTIVE: bool = false;

#[cfg(all(
    feature = "jemalloc",
    not(target_env = "msvc"),
    not(target_os = "android")
))]
#[global_allocator]
static GLOBAL_ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

/// jemalloc 运行时配置，通过导出的 `malloc_conf` 符号生效。
///
/// - `background_thread:true`：脏页回收交给后台线程，避免回收延迟落在请求路径上
/// - `narenas:2`：扩展进程实际并发度很低，减少 arena 数量以提升缓存局部性
/// - `dirty_decay_ms` / `muzzy_decay_ms`：10s 衰减，兼顾常驻内存与峰值复用
#[cfg(all(
    feature = "jemalloc",
    not(target_env = "msvc"),
    not(target_os = "android")
))]
#[export_name = "malloc_conf"]
pub static MALLOC_CONF: &[u8] =
    b"background_thread:true,narenas:2,dirty_decay_ms:10000,muzzy_decay_ms:10000\0";

/// 当前生效的全局分配器名称，用于诊断输出。
pub const fn name() -> &'static str {
    if JEMALLOC_ACTIVE {
        "tikv-jemallocator"
    } else {
        "system"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_matches_activation_flag() {
        if JEMALLOC_ACTIVE {
            assert_eq!(name(), "tikv-jemallocator");
        } else {
            assert_eq!(name(), "system");
        }
    }

    #[test]
    fn malloc_conf_is_nul_terminated() {
        // jemalloc 要求配置串以 NUL 结尾，否则会读取越界内存。
        if JEMALLOC_ACTIVE {
            assert_eq!(MALLOC_CONF.last(), Some(&0u8));
            assert!(!MALLOC_CONF[..MALLOC_CONF.len() - 1].contains(&0u8));
        }
    }
}
