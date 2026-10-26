//! HookRegistry：定义集合 + 事件索引 + 层合并。
//!
//! 同名冲突优先级（设计文档 §5.3）：**user > project > plugin**——
//! 用户显式配置覆盖 plugin 捆绑的同名 hook。同层同名取先声明并 warn。

use crate::definition::HookDefinition;
use crate::event::HookEventKind;
use std::collections::HashMap;
use std::sync::Arc;

/// 层优先级数值：越大越高。plugin < project < user。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum LayerRank {
    Plugin = 0,
    Project = 1,
    User = 2,
}

impl LayerRank {
    fn of(scope: &crate::definition::HookScope) -> Self {
        match scope {
            crate::definition::HookScope::User => Self::User,
            crate::definition::HookScope::Project => Self::Project,
            crate::definition::HookScope::Plugin(_) => Self::Plugin,
        }
    }
}

/// 不可变注册表（构建后只读；`rebuild` 走整体替换）。
#[derive(Default)]
pub struct HookRegistry {
    defs: Vec<Arc<HookDefinition>>,
    index: HashMap<HookEventKind, Vec<Arc<HookDefinition>>>,
}

impl HookRegistry {
    /// 三层合并。层内保留声明序；`all()` 的顺序为 user → project → plugin。
    pub fn from_layers(
        user: Vec<HookDefinition>,
        project: Vec<HookDefinition>,
        plugin: Vec<HookDefinition>,
    ) -> Self {
        // 低优先级层先入列，高优先级层同名替换（保持原位，稳定输出序）。
        let mut defs: Vec<Arc<HookDefinition>> = Vec::new();
        let mut ranks: Vec<LayerRank> = Vec::new();
        let mut name_pos: HashMap<String, usize> = HashMap::new();

        for (layer_defs, default_rank) in [
            (plugin, LayerRank::Plugin),
            (project, LayerRank::Project),
            (user, LayerRank::User),
        ] {
            for def in layer_defs {
                let rank = LayerRank::of(&def.scope).max(default_rank);
                match name_pos.get(&def.name) {
                    Some(&pos) => {
                        if ranks[pos] == rank {
                            tracing::warn!(
                                hook = %def.name,
                                scope = %def.scope.label(),
                                "duplicate hook name within same layer; first declaration wins"
                            );
                        } else if ranks[pos] < rank {
                            tracing::info!(
                                hook = %def.name,
                                scope = %def.scope.label(),
                                "hook overridden by higher-priority layer"
                            );
                            defs[pos] = Arc::new(def);
                            ranks[pos] = rank;
                        }
                        // ranks[pos] > rank：低优先级层的新同名条目直接丢弃
                    }
                    None => {
                        name_pos.insert(def.name.clone(), defs.len());
                        defs.push(Arc::new(def));
                        ranks.push(rank);
                    }
                }
            }
        }

        // defs 现在的顺序是 plugin → project → plugin…（按插入序，被替换的保持原位）。
        // 重排为 user → project → plugin（rank 降序，稳定排序），保证 `all()`
        // 与事件桶内顺序可预测：用户配置先执行。
        let mut order: Vec<usize> = (0..defs.len()).collect();
        order.sort_by(|&a, &b| ranks[b].cmp(&ranks[a]).then(a.cmp(&b)));
        let defs: Vec<Arc<HookDefinition>> = order.into_iter().map(|i| defs[i].clone()).collect();

        let mut index: HashMap<HookEventKind, Vec<Arc<HookDefinition>>> = HashMap::new();
        for def in &defs {
            for kind in &def.events {
                index.entry(*kind).or_default().push(def.clone());
            }
        }

        Self { defs, index }
    }

    /// 某事件的全部订阅者（user → project → plugin 序）。
    pub fn subscribers(&self, kind: HookEventKind) -> &[Arc<HookDefinition>] {
        self.index.get(&kind).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// 全部定义（展示/diagnostics 用）。
    pub fn all(&self) -> &[Arc<HookDefinition>] {
        &self.defs
    }

    pub fn is_empty(&self) -> bool {
        self.defs.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::definition::{HookAction, HookScope, OnFailure};
    use std::time::Duration;

    fn def(name: &str, scope: HookScope, event: HookEventKind, marker: &str) -> HookDefinition {
        HookDefinition {
            name: name.into(),
            events: vec![event],
            match_tool: None,
            action: HookAction::Command(vec![marker.to_string()]),
            timeout: Duration::from_millis(1000),
            on_failure: OnFailure::Warn,
            include_subagents: false,
            scope,
        }
    }

    fn command_marker(d: &HookDefinition) -> String {
        match &d.action {
            HookAction::Command(argv) => argv[0].clone(),
            HookAction::Builtin(_) => "builtin".into(),
        }
    }

    #[test]
    fn layer_priority_user_beats_plugin() {
        let registry = HookRegistry::from_layers(
            vec![def(
                "x",
                HookScope::User,
                HookEventKind::ToolPost,
                "user-version",
            )],
            vec![],
            vec![def(
                "x",
                HookScope::Plugin("p".into()),
                HookEventKind::ToolPost,
                "plugin-version",
            )],
        );
        let subs = registry.subscribers(HookEventKind::ToolPost);
        assert_eq!(subs.len(), 1);
        assert_eq!(command_marker(&subs[0]), "user-version"); // user 版本胜出
        assert_eq!(registry.all().len(), 1);
    }

    #[test]
    fn layer_priority_user_beats_project_beats_plugin() {
        let registry = HookRegistry::from_layers(
            vec![def("x", HookScope::User, HookEventKind::ToolPost, "u")],
            vec![def("x", HookScope::Project, HookEventKind::ToolPost, "p")],
            vec![def(
                "x",
                HookScope::Plugin("p".into()),
                HookEventKind::ToolPost,
                "g",
            )],
        );
        let subs = registry.subscribers(HookEventKind::ToolPost);
        assert_eq!(subs.len(), 1);
        assert_eq!(command_marker(&subs[0]), "u");
        assert_eq!(registry.all()[0].scope, HookScope::User);
    }

    #[test]
    fn same_layer_dup_first_wins() {
        let registry = HookRegistry::from_layers(
            vec![
                def("x", HookScope::User, HookEventKind::ToolPost, "first"),
                def("x", HookScope::User, HookEventKind::ToolPost, "second"),
            ],
            vec![],
            vec![],
        );
        assert_eq!(registry.all().len(), 1);
        assert_eq!(command_marker(&registry.all()[0]), "first");
    }

    #[test]
    fn index_buckets_by_event() {
        let registry = HookRegistry::from_layers(
            vec![
                def("a", HookScope::User, HookEventKind::ToolPre, "a"),
                def("b", HookScope::User, HookEventKind::TurnStart, "b"),
                def("c", HookScope::User, HookEventKind::ToolPre, "c"),
            ],
            vec![],
            vec![],
        );
        let pre = registry.subscribers(HookEventKind::ToolPre);
        assert_eq!(pre.len(), 2);
        assert!(registry.subscribers(HookEventKind::ToolPost).is_empty());
        // all() 按 user → project → plugin + 声明序
        assert_eq!(registry.all().len(), 3);
    }

    #[test]
    fn ordering_user_before_plugin() {
        let registry = HookRegistry::from_layers(
            vec![def("u", HookScope::User, HookEventKind::ToolPost, "u")],
            vec![],
            vec![def(
                "p",
                HookScope::Plugin("p".into()),
                HookEventKind::ToolPost,
                "p",
            )],
        );
        let subs = registry.subscribers(HookEventKind::ToolPost);
        assert_eq!(subs.len(), 2);
        assert_eq!(subs[0].name, "u");
        assert_eq!(subs[1].name, "p");
    }
}
