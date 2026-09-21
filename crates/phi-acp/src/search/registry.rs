//! 算法注册表 —— 对应 `src/search/registry.ts`。
//!
//! 内置算法预先注册；宿主也可以注册额外算法（如 embedding 语义检索）并按名字
//! 在 [`crate::search::SearchOptions::algorithm`] 里引用。
//!
//! 扩展回调跑在单线程 runtime 上，用 `thread_local` 足够；顺带让单测互不干扰。

use std::cell::RefCell;
use std::rc::Rc;

use super::algorithms::bm25::Bm25;
use super::algorithms::fuzzy::Fuzzy;
use super::algorithms::hybrid::Hybrid;
use super::algorithms::substring::Substring;
use super::types::SearchAlgorithm;

thread_local! {
    /// 已注册算法（内置四个，对应上游 `registry.ts` 的预注册顺序）。
    static REGISTRY: RefCell<Vec<Rc<dyn SearchAlgorithm>>> = RefCell::new(vec![
        Rc::new(Substring),
        Rc::new(Bm25),
        Rc::new(Fuzzy),
        Rc::new(Hybrid),
    ]);
}

/// 注册（或按同名覆盖）一个算法。
pub fn register_search_algorithm(algorithm: Rc<dyn SearchAlgorithm>) {
    REGISTRY.with(|registry| {
        let mut registry = registry.borrow_mut();
        let name = algorithm.name();
        registry.retain(|existing| existing.name() != name);
        registry.push(algorithm);
    });
}

/// 按名字取算法。
pub fn get_search_algorithm(name: &str) -> Option<Rc<dyn SearchAlgorithm>> {
    REGISTRY.with(|registry| {
        registry
            .borrow()
            .iter()
            .find(|algorithm| algorithm.name() == name)
            .cloned()
    })
}

/// 列出全部已注册算法。
pub fn list_search_algorithms() -> Vec<Rc<dyn SearchAlgorithm>> {
    REGISTRY.with(|registry| registry.borrow().clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::types::{ScoredDoc, SearchDoc};

    struct Marker;

    impl SearchAlgorithm for Marker {
        fn name(&self) -> &'static str {
            "marker"
        }

        fn description(&self) -> &'static str {
            "test-only"
        }

        fn score(&self, docs: &[SearchDoc], _query: &str) -> Vec<ScoredDoc> {
            docs.iter()
                .map(|doc| ScoredDoc {
                    reference: doc.reference.clone(),
                    score: 42.0,
                })
                .collect()
        }
    }

    #[test]
    fn builtins_should_be_registered() {
        for name in ["substring", "bm25", "fuzzy", "hybrid"] {
            assert!(get_search_algorithm(name).is_some(), "{name} 未注册");
        }
        assert_eq!(list_search_algorithms().len(), 4);
    }

    #[test]
    fn unknown_algorithm_should_be_none() {
        assert!(get_search_algorithm("nope").is_none());
    }

    #[test]
    fn registering_the_same_name_should_override() {
        register_search_algorithm(Rc::new(Marker));
        assert!(get_search_algorithm("marker").is_some());
        // 同名再注册一次不得留下两条。
        register_search_algorithm(Rc::new(Marker));
        let matches = list_search_algorithms()
            .iter()
            .filter(|a| a.name() == "marker")
            .count();
        assert_eq!(matches, 1);
    }
}
