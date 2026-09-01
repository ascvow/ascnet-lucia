//! 插件平面的宿主侧装配：启动加载、人工管理服务与宿主动作处理。

pub(crate) mod host_actions;
pub(crate) mod management;
pub(crate) mod recovery;
pub(crate) mod startup;

pub(crate) use management::*;
