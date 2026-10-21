//! 集成测试入口。
//!
//! Cargo 把 `tests/integration/main.rs` 识别为一个名为 `integration` 的测试目标，
//! 下面的每个模块都是这个目标里的一个测试分组。
//!
//! 分组：
//! - [`support`]：共享的假平台与构造器。
//! - [`bot_flow`]：事件派发与回复流程。
//! - [`provider_contract`]：小黑盒实现的纯函数契约。

mod bot_flow;
mod provider_contract;
mod support;
