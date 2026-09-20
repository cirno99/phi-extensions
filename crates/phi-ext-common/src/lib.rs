//! phi 扩展共享工具库。
//!
//! 本 crate 不依赖 `phi-ext`，只提供纯逻辑工具（路径解析、配置读写、
//! ANSI 处理、文本格式化、用量统计、竞技场分配器），因此可以被单元测试完整覆盖。
//!
//! 同时它承载两处与分配相关的全局设施：
//! - [`alloc`]：`tikv-jemallocator` 全局分配器，随本 crate 自动链接进每个扩展
//! - [`arena`]：`bumpalo` 竞技场，供单次调用内部的临时分配复用

pub mod alloc;
#[cfg(feature = "arena")]
pub mod arena;
pub mod ansi;
pub mod config;
pub mod paths;
pub mod stats;
pub mod text;
pub mod time;
#[cfg(feature = "simd")]
pub mod json;

use std::cell::RefCell;
use std::rc::Rc;

/// 扩展内部共享状态别名。
///
/// phi 的钩子回调签名是 `FnMut(Event) -> Option<Result> + 'static`，不带
/// `Send` 约束，且无法访问 `Context`。因此多个回调之间共享状态的标准做法
/// 是用 `Rc<RefCell<T>>` 克隆进每个闭包。
///
/// 注意：回调执行在单线程 runtime 上，`Rc<RefCell<T>>` 是安全的；
/// 但必须避免在持有 `borrow_mut()` 时再次借用同一个 `RefCell`。
pub type Shared<T> = Rc<RefCell<T>>;

/// 创建共享状态。
pub fn shared<T>(value: T) -> Shared<T> {
    Rc::new(RefCell::new(value))
}
