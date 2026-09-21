//! 每篇文档的派生特征，跨检索调用记忆化 —— 对应 acp-kernel `src/search/doc-cache.ts`。
//!
//! 检索的是**同一批不可变文档**（压缩块摘要 + 已折叠的消息原文），每次调用都
//! 重新打分。没有这层缓存时，每次 `acp_search` 都要把整个语料重新分词（CJK 段
//! 冷启动约 0.3s/MB）、重新小写化、重建 bigram 集合——5MB 会话每次调用约 3s，
//! 且随会话长度线性增长。有了缓存，语料只处理一次，后续检索是
//! O(文档数 × 查询词数)。
//!
//! 以文档原文为键（原文不可变）。容量按**缓存源字符总数**封顶，超限时淘汰最旧
//! 的条目，因此长驻进程服务多个会话也不会无界增长。会话切换时可调
//! [`clear_doc_features`] 主动释放（可选：封顶本身已经限制了它）。

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;

use super::tokenizer::{char_bigrams, tf_map};

/// 一篇文档的派生特征。
#[derive(Debug)]
pub struct DocFeatures {
    /// 词干化后的词频（BM25 通道）。
    pub tf: HashMap<String, u32>,
    /// 词总数（BM25 长度归一化）。
    pub len: u32,
    /// 小写化后的文本（substring + fuzzy 通道）。
    pub lower: String,
    /// `lower` 的去重字符 bigram（fuzzy 通道）。
    pub grams: HashSet<String>,
}

/// 默认缓存上限（源字符数），对应上游 `DEFAULT_CAP_CHARS = 8 * 1024 * 1024`。
const DEFAULT_CAP_CHARS: usize = 8 * 1024 * 1024;

#[derive(Debug)]
struct DocCache {
    cap: usize,
    chars: usize,
    /// 插入顺序（FIFO 淘汰）。
    order: VecDeque<String>,
    map: HashMap<String, Rc<DocFeatures>>,
}

impl Default for DocCache {
    fn default() -> Self {
        Self {
            cap: DEFAULT_CAP_CHARS,
            chars: 0,
            order: VecDeque::new(),
            map: HashMap::new(),
        }
    }
}

thread_local! {
    static CACHE: RefCell<DocCache> = RefCell::new(DocCache::default());
}

fn build(text: &str) -> DocFeatures {
    let tf = tf_map(text, true);
    let len = tf.values().sum();
    let lower = text.to_lowercase();
    DocFeatures {
        tf,
        len,
        grams: char_bigrams(&lower).into_iter().collect(),
        lower,
    }
}

/// 取一篇文档的派生特征（命中缓存则直接复用）。
pub fn doc_features(text: &str) -> Rc<DocFeatures> {
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(hit) = cache.map.get(text) {
            return Rc::clone(hit);
        }
        let features = Rc::new(build(text));
        let len = text.len();
        if len > 0 && len <= cache.cap {
            while cache.chars + len > cache.cap {
                let Some(oldest) = cache.order.pop_front() else {
                    break;
                };
                cache.chars = cache.chars.saturating_sub(oldest.len());
                cache.map.remove(&oldest);
            }
            cache.order.push_back(text.to_string());
            cache.chars += len;
            cache.map.insert(text.to_string(), Rc::clone(&features));
        }
        features
    })
}

/// 清空全部缓存（会话关闭 / 切换时调用）。
pub fn clear_doc_features() {
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        cache.map.clear();
        cache.order.clear();
        cache.chars = 0;
    });
}

/// 设置缓存上限（源字符数）。比上限大的文档永不缓存。
pub fn set_doc_cache_cap(chars: usize) {
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        cache.cap = chars.max(1);
        while cache.chars > cache.cap {
            let Some(oldest) = cache.order.pop_front() else {
                break;
            };
            cache.chars = cache.chars.saturating_sub(oldest.len());
            cache.map.remove(&oldest);
        }
    });
}

/// 缓存占用（条目数, 源字符数）——诊断用。
pub fn doc_cache_info() -> (usize, usize) {
    CACHE.with(|cache| {
        let cache = cache.borrow();
        (cache.map.len(), cache.chars)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 单测共享同一个 thread_local 缓存，必须串行并各自复位。
    fn with_clean_cache<F: FnOnce()>(cap: usize, body: F) {
        clear_doc_features();
        set_doc_cache_cap(cap);
        body();
        clear_doc_features();
        set_doc_cache_cap(DEFAULT_CAP_CHARS);
    }

    #[test]
    fn features_should_expose_tf_length_lower_and_grams() {
        with_clean_cache(DEFAULT_CAP_CHARS, || {
            let features = doc_features("Auth Token auth");
            assert_eq!(features.lower, "auth token auth");
            assert_eq!(features.len, 3);
            assert_eq!(features.tf.get("auth"), Some(&2));
            assert_eq!(features.tf.get("token"), Some(&1));
            assert!(features.grams.contains("au"));
        });
    }

    #[test]
    fn features_should_be_memoized_by_text() {
        with_clean_cache(DEFAULT_CAP_CHARS, || {
            let first = doc_features("memoized text");
            let second = doc_features("memoized text");
            assert!(Rc::ptr_eq(&first, &second), "同一文本必须复用同一份特征");
            assert_eq!(doc_cache_info().0, 1);
        });
    }

    #[test]
    fn cache_should_evict_oldest_when_cap_exceeded() {
        // 每篇 5 字符，上限 10 ⇒ 最多留 2 篇。
        with_clean_cache(10, || {
            doc_features("aaaaa");
            doc_features("bbbbb");
            assert_eq!(doc_cache_info(), (2, 10));
            doc_features("ccccc");
            // 最旧的 aaaaa 被淘汰。
            assert_eq!(doc_cache_info(), (2, 10));
            let fresh = doc_features("bbbbb");
            let rebuilt = Rc::new(build("bbbbb"));
            // 命中缓存（未被淘汰）时不应重建。
            assert_eq!(fresh.len, rebuilt.len);
            assert_eq!(doc_cache_info().0, 2);
        });
    }

    #[test]
    fn oversized_doc_should_never_be_cached() {
        with_clean_cache(4, || {
            doc_features("way too long for the cap");
            assert_eq!(doc_cache_info(), (0, 0));
        });
    }

    #[test]
    fn shrinking_cap_should_evict() {
        with_clean_cache(DEFAULT_CAP_CHARS, || {
            doc_features("aaaaa");
            doc_features("bbbbb");
            doc_features("ccccc");
            assert_eq!(doc_cache_info().0, 3);
            set_doc_cache_cap(6);
            assert_eq!(doc_cache_info(), (1, 5));
        });
    }
}
