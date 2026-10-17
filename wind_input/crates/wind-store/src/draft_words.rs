//! 自动造词的草稿层（`Runtime` 的草稿 API 门面；真源是 `data/<schema>/draft.dict.yaml`）
//!
//! 滑窗切出的候选词先落这里，用户真的用它上屏过一次才跃迁进临时词库
//! （[`Store::promote_draft_to_temp`]）。设计见 `docs/design/auto-phrase-draft-layer.md`。
//!
//! # 与临时词库的分界
//!
//! **谁验证过它**：草稿是机器猜的，带有效期，到期没人用就丢；临时词是用户用过的，长期留着。
//! 杂词率高是本层的**预期而非缺陷**——整个模型的重心就是「先记一堆、用过的才留」，
//! 过滤发生在使用端而不是产生端。
//!
//! # 时间语义
//!
//! 引擎侧只持久化一个时间列 `last_used`（文本表 `text, code, last_used` 也只有它），
//! 而草稿**写入不续期**：同一条草稿已存在且未过期时直接跳过（见 [`Store::add_drafts`]），
//! 于是 `last_used` 在草稿上就是**首次入库时间**。容量淘汰按它升序 ⇒ 纯 FIFO，
//! 反复打的词不会因为「又被写了一次」而排到后面；过期条目重新打出时会被删掉重建，
//! 拿到一个全新的有效期。
//!
//! ⚠️ 本层的过期判定**必须在查询时做**（[`Store::search_drafts`] 带 `ttl_secs`），
//! 不能只靠 [`Store::purge_expired_drafts`] 定期清理：清理线程没跑到的窗口里，
//! 过期草稿照样会被召回。清理只是为了不让表无限堆积，不是过期语义的实现。

use crate::store::Store;
use crate::user_words::split_key;

/// 旧表 `DRAFT_WORDS` 的 value：定长 8 字节 —— `created_at i64`（P1 迁移读它）。
pub(crate) fn dec_draft(b: &[u8]) -> Option<i64> {
    if b.len() < 8 {
        return None;
    }
    Some(i64::from_le_bytes(b[0..8].try_into().ok()?))
}

impl Store {
    /// 批量写入草稿（一次 `apply_batch` 落库）。已存在且**未过期**的条目**跳过**（不续期）；
    /// 已过期的旧记录删掉重建，等同一条全新草稿（见模块文档「时间语义」）。
    ///
    /// `ttl_secs` 就是本层的有效期配置（`auto_phrase.draft_ttl_hours * 3600`），它在这里
    /// 回答「库里那条同 key 的记录还算不算存在」；`0` = 永不过期，已存在的一律保留。
    ///
    /// 批量是刚性要求而非优化：草稿的产生速率（滑窗下约每字 4 条）远高于这个库里的其它
    /// 写入方，调用方负责攒批，本函数保证「这一批共用一次批量写」。
    ///
    /// 返回**实际落库条数**（被跳过的已存在条目不计入）。
    pub fn add_drafts(
        &self,
        schema: &str,
        items: &[(String, String)],
        ttl_secs: i64,
    ) -> anyhow::Result<usize> {
        let n = self.runtime().add_drafts(schema, items, ttl_secs)?;
        self.after_write();
        Ok(n)
    }

    /// 精确查某 code 下**未过期**的草稿，返回 text 列表。
    ///
    /// 只做精确查询，**没有前缀版本**，这是设计的一部分而非省略：码表打字是前缀式的
    /// （w → wq → wqv…），草稿若参与前缀召回，滑窗那些杂词会在每一次按键上涌进候选列表。
    /// 只精确命中意味着「打满这个词的完整码才出来」，正是草稿该有的语义。
    pub fn search_drafts(
        &self,
        schema: &str,
        code: &str,
        ttl_secs: i64,
    ) -> anyhow::Result<Vec<String>> {
        Ok(self.runtime().search_drafts(schema, code, ttl_secs))
    }

    /// 草稿是否存在且未过期（跃迁前的判据）。
    pub fn has_draft(&self, schema: &str, code: &str, text: &str, ttl_secs: i64) -> bool {
        self.runtime().has_draft(schema, code, text, ttl_secs)
    }

    /// 清理过期草稿（引擎侧批量删）。返回删除条数。`ttl_secs == 0` 时什么都不做。
    ///
    /// 这是**容量维护**，不是过期语义——过期判定在 [`Self::search_drafts`] 里，
    /// 少跑一次清理只会让表大一点，不会让过期草稿被召回。
    pub fn purge_expired_drafts(&self, schema: &str, ttl_secs: i64) -> anyhow::Result<usize> {
        Ok(self
            .runtime()
            .purge_expired_drafts(Some(schema), ttl_secs)?
            .0)
    }

    /// 同上，但**不分方案扫全表**。启动时跑一次。
    ///
    /// 按方案那一版永远够不着两类表：**开关被关掉之后**（此后再没有落库，而清理只跟在
    /// 落库之后跑）、以及**用户已经不用的方案**（它的前缀再也不会被带上）。两类都是
    /// 「草稿永久躺在库里」，而它们恰恰是最该被收掉的——没人会再用到它们。
    ///
    /// 不看开关是刻意的：关掉功能的用户更需要这次清理，且因为没有新的写入，
    /// 他的草稿表会在一个 TTL 内自行排空。
    ///
    /// 返回 `(删除条数, 剩余条数)`。剩余条数是**顺带**数出来的——这次扫描本来就要
    /// 走遍全表，而它是真机重定 `draft_max_entries` 唯一的观测出口（那个默认值目前
    /// 还是纸面估算）。`ttl_secs == 0` 时不扫描，返回 `(0, 0)`。
    pub fn purge_all_expired_drafts(&self, ttl_secs: i64) -> anyhow::Result<(usize, usize)> {
        self.runtime().purge_expired_drafts(None, ttl_secs)
    }

    /// 容量淘汰：保留 `max_keep` 条，按 `last_used` **升序**淘汰（最早的先走）。返回淘汰条数。
    ///
    /// 草稿上 `last_used` 是**首次入库时间**（写入不续期，见 [`Self::add_drafts`]），所以这条
    /// 升序是纯 FIFO，不是近似 LRU。排序键只有它，因为草稿没有 count 可比——它一旦被用过
    /// 就不再是草稿了（跃迁进临时词库，见 [`Self::promote_draft_to_temp`]）。
    pub fn evict_drafts(&self, schema: &str, max_keep: usize) -> anyhow::Result<usize> {
        self.runtime().evict_drafts(schema, max_keep)
    }

    /// 跃迁：草稿 → 临时词库（**单写事务**）。草稿不存在时返回 `false`，什么都不做。
    ///
    /// 「用过即转正」的第一跳。此后它就是一条普通临时词：再被用到 `count++`，
    /// 累到 `promote_count` 晋升进用户词库，容量超限时按 `(count, created_at, weight)` 淘汰。
    ///
    /// ⚠️ **必须同时维护 `TEMP_ABBREV` 索引**：主表写了、索引没写 ⇒ 那个词的简拼静默召不回。
    /// 本仓的教训是「`promote_temp_word` 写的是用户词表却住在 temp_words.rs，按文件名数必漏」
    /// （`3ff3f1fc`），这里是同一类陷阱的镜像——本函数住在 draft_words.rs，写的却是临时词表。
    ///
    /// 已存在同 `(schema, code, text)` 的临时词时**沿用旧记录并 `count++`**，不覆盖它的
    /// weight 与 created_at：那条已经是用户用过的词，草稿没有资格把它的历史抹掉。
    pub fn promote_draft_to_temp(
        &self,
        schema: &str,
        code: &str,
        text: &str,
        add_weight: i32,
        boundary: u64,
    ) -> anyhow::Result<bool> {
        let ok = self
            .runtime()
            .promote_draft_to_temp(schema, code, text, add_weight, boundary)?;
        if ok {
            self.bump_words_gen(schema);
            self.after_write();
        }
        Ok(ok)
    }

    /// 某方案当前的草稿条数（含已过期未清理的）。供容量维护与实测调参。
    pub fn count_drafts(&self, schema: &str) -> anyhow::Result<usize> {
        Ok(self.runtime().count_drafts(schema))
    }

    /// 清空某方案的全部草稿。换词库/换方案配置后调用——草稿的码是按旧词库算的，留着就是错码。
    pub fn clear_drafts(&self, schema: &str) -> anyhow::Result<usize> {
        self.runtime().clear_drafts(schema)
    }
}

/// 拆 key 供调试/导出用（与 `user_words::split_key` 同一套编码）。
pub fn parse_draft_key(key: &str) -> Option<(&str, &str, &str)> {
    split_key(key)
}

#[cfg(test)]
pub(crate) fn now_for_test() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use now_for_test as now_secs;

    /// 以给定时刻写草稿（模拟「很久以前写的」）：P1 起草稿在运行时引擎，有效期基准是 last_used。
    fn drafts_at(s: &Store, rows: &[(&str, &str, &str, i64)]) {
        use wind_userdata::engine::Op;
        use wind_userdata::record::{OpKind, Payload, Target};
        let ops: Vec<Op> = rows
            .iter()
            .map(|(schema, code, text, ca)| {
                let mut op = Op::new(
                    Target::Draft,
                    schema,
                    OpKind::Upsert,
                    code,
                    text,
                    Payload::Weight(0),
                    0,
                );
                op.ts_ms = ca * 1000;
                op
            })
            .collect();
        s.runtime().data().apply_batch(&ops).unwrap();
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(name);
        let _ = std::fs::remove_file(&p);
        p
    }

    fn items(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter()
            .map(|(c, t)| (c.to_string(), t.to_string()))
            .collect()
    }

    /// 写入 → 精确召回；**前缀查不到**（草稿只精确命中，见 `search_drafts` 的文档）。
    #[test]
    fn drafts_are_found_by_exact_code_only() {
        let p = tmp("wind_draft_exact.redb");
        let s = Store::open(&p).unwrap();
        s.add_drafts("wb", &items(&[("wqvb", "你好"), ("wqv", "你女")]), 0)
            .unwrap();
        assert_eq!(s.search_drafts("wb", "wqvb", 0).unwrap(), vec!["你好"]);
        assert_eq!(s.search_drafts("wb", "wqv", 0).unwrap(), vec!["你女"]);
        // 不是任何一条的完整码 ⇒ 什么都不该出来（前缀不召回）
        assert!(s.search_drafts("wb", "wq", 0).unwrap().is_empty());
        let _ = std::fs::remove_file(&p);
    }

    /// **过期判定必须在查询里**，不能只靠定期清理——否则清理没跑到的窗口里过期草稿照样被召回。
    ///
    /// 用带时刻的操作构造隔天的草稿：`add_drafts` 取的是当下秒数，公共 API 造不出这个场景。
    #[test]
    fn expired_drafts_are_filtered_at_query_time_not_only_by_purge() {
        let p = tmp("wind_draft_ttl.redb");
        let s = Store::open(&p).unwrap();
        let now = now_secs();
        drafts_at(
            &s,
            &[
                ("wb", "aaaa", "刚记的", now - 10),
                ("wb", "bbbb", "昨天的", now - 90_000), // > 1 天
            ],
        );
        let day = 86_400;
        assert_eq!(s.search_drafts("wb", "aaaa", day).unwrap(), vec!["刚记的"]);
        assert!(
            s.search_drafts("wb", "bbbb", day).unwrap().is_empty(),
            "过期草稿必须在查询时就被滤掉，不能等清理"
        );
        // ttl=0 ＝ 永不过期
        assert_eq!(s.search_drafts("wb", "bbbb", 0).unwrap(), vec!["昨天的"]);
        // 清理只删过期的那条
        assert_eq!(s.purge_expired_drafts("wb", day).unwrap(), 1);
        assert_eq!(s.count_drafts("wb").unwrap(), 1);
        let _ = std::fs::remove_file(&p);
    }

    /// 再次写入同一条草稿**不续期**：`last_used` 永远是**首次**入库时间。
    ///
    /// 于是容量淘汰是纯 FIFO——最早进草稿的先走，与它被重复写过几次无关。
    #[test]
    fn rewriting_a_draft_keeps_its_first_seen_time() {
        let p = tmp("wind_draft_no_renew.redb");
        let s = Store::open(&p).unwrap();
        let now = now_secs();
        // 三条，首次入库时间递增；其中最老的那条稍后被「再打一次」
        drafts_at(
            &s,
            &[
                ("wb", "aaaa", "最老", now - 300),
                ("wb", "bbbb", "中间", now - 200),
                ("wb", "cccc", "最新", now - 100),
            ],
        );
        // 「最老」被再次打出 ⇒ 跳过，首次入库时间不动，这次一条也没落库
        assert_eq!(
            s.add_drafts("wb", &items(&[("aaaa", "最老")]), 86_400)
                .unwrap(),
            0,
            "已存在且未过期的草稿不该被重写"
        );
        // 保留 2 条 ⇒ 该淘汰的仍然是「最老」：它没有因为又被写了一次而排到后面
        assert_eq!(s.evict_drafts("wb", 2).unwrap(), 1);
        assert!(
            s.search_drafts("wb", "aaaa", 0).unwrap().is_empty(),
            "没续期的草稿仍是最老的，该被淘汰"
        );
        assert!(!s.search_drafts("wb", "bbbb", 0).unwrap().is_empty());
        assert!(!s.search_drafts("wb", "cccc", 0).unwrap().is_empty());
        let _ = std::fs::remove_file(&p);
    }

    /// 已**过期**的旧记录等同于不存在：重新打出同一个词 = 一条全新的草稿，重新获得
    /// 一整个有效期。否则清理（机会性的，只跟在落库之后）没扫到之前，那条词再也进不了草稿。
    #[test]
    fn an_expired_draft_is_rewritten_as_a_brand_new_one() {
        let p = tmp("wind_draft_expired_rewrite.redb");
        let s = Store::open(&p).unwrap();
        let now = now_secs();
        let day = 86_400;
        drafts_at(&s, &[("wb", "aaaa", "隔天的", now - 90_000)]);
        // 过期 ⇒ 不该被当成「已存在」，本次真的落库
        assert_eq!(
            s.add_drafts("wb", &items(&[("aaaa", "隔天的")]), day)
                .unwrap(),
            1
        );
        assert_eq!(
            s.search_drafts("wb", "aaaa", day).unwrap(),
            vec!["隔天的"],
            "过期之后重新打出 = 新草稿，该能被召回"
        );
        // 这回它没过期 ⇒ 再次写入被跳过
        assert_eq!(
            s.add_drafts("wb", &items(&[("aaaa", "隔天的")]), day)
                .unwrap(),
            0
        );
        // ttl=0 ＝ 永不过期 ⇒ 已存在的条目一律保留，同样不重写
        assert_eq!(
            s.add_drafts("wb", &items(&[("aaaa", "隔天的")]), 0)
                .unwrap(),
            0
        );
        let _ = std::fs::remove_file(&p);
    }

    /// 跃迁：草稿消失、临时词出现，且**简拼索引必须跟着建**。
    #[test]
    fn promoting_a_draft_moves_it_and_maintains_the_abbrev_index() {
        let p = tmp("wind_draft_promote.redb");
        let s = Store::open(&p).unwrap();
        s.add_drafts("py", &items(&[("nihao", "你好")]), 0).unwrap();
        assert!(
            s.promote_draft_to_temp("py", "nihao", "你好", 800, 0b101)
                .unwrap()
        );
        assert!(
            s.search_drafts("py", "nihao", 0).unwrap().is_empty(),
            "跃迁后草稿该消失"
        );
        let temp = s.get_temp_words("py", "nihao").unwrap();
        assert_eq!(temp.len(), 1);
        assert_eq!(temp[0].text, "你好");
        assert_eq!(temp[0].count, 1);
        assert!(
            !s.search_temp_words_by_abbrev("py", "nh", 0)
                .unwrap()
                .is_empty(),
            "主表写了索引没写 ⇒ 这个词的简拼静默召不回"
        );
        // 不存在的草稿：什么都不做
        assert!(
            !s.promote_draft_to_temp("py", "nihao", "你好", 800, 0b101)
                .unwrap()
        );
        let _ = std::fs::remove_file(&p);
    }

    /// 跃迁到一条**已存在**的临时词上：count++，不覆盖它的 weight 与历史。
    #[test]
    fn promoting_onto_an_existing_temp_word_bumps_count_without_erasing_history() {
        let p = tmp("wind_draft_promote_existing.redb");
        let s = Store::open(&p).unwrap();
        s.learn_temp_word("wb", "wqvb", "你好", 1234, 0).unwrap(); // 已有、权重更高
        s.add_drafts("wb", &items(&[("wqvb", "你好")]), 0).unwrap();
        assert!(
            s.promote_draft_to_temp("wb", "wqvb", "你好", 800, 0)
                .unwrap()
        );
        let temp = s.get_temp_words("wb", "wqvb").unwrap();
        assert_eq!(temp.len(), 1);
        assert_eq!(temp[0].count, 2, "该是 count++ 而不是重置");
        assert_eq!(
            temp[0].weight, 1234,
            "草稿没有资格把用户用过的词的权重压下去"
        );
        let _ = std::fs::remove_file(&p);
    }

    /// 方案隔离：一个方案的草稿不该被另一个方案看见或误删。
    #[test]
    fn drafts_are_isolated_per_schema() {
        let p = tmp("wind_draft_schema.redb");
        let s = Store::open(&p).unwrap();
        s.add_drafts("wb", &items(&[("aaaa", "五笔的")]), 0)
            .unwrap();
        s.add_drafts("py", &items(&[("aaaa", "拼音的")]), 0)
            .unwrap();
        assert_eq!(s.search_drafts("wb", "aaaa", 0).unwrap(), vec!["五笔的"]);
        assert_eq!(s.clear_drafts("wb").unwrap(), 1);
        assert_eq!(
            s.search_drafts("py", "aaaa", 0).unwrap(),
            vec!["拼音的"],
            "清空一个方案不该动到另一个"
        );
        let _ = std::fs::remove_file(&p);
    }

    /// 启动清理要**跨方案**：按方案清永远够不着「用户已经不用了的方案」留下的草稿。
    ///
    /// 这正是加这一条通路的理由——落库后的清理只会带上当前方案的前缀。
    #[test]
    fn startup_purge_sweeps_every_schema_not_just_the_active_one() {
        let p = tmp("wind_draft_purge_all.redb");
        let s = Store::open(&p).unwrap();
        let now = now_secs();
        drafts_at(
            &s,
            &[
                ("wb", "aaaa", "五笔新的", now - 10),
                ("wb", "bbbb", "五笔旧的", now - 90_000),
                ("py", "cccc", "拼音旧的", now - 90_000),
                ("zr", "dddd", "自然旧的", now - 90_000),
            ],
        );
        let day = 86_400;
        // 对照：按方案清只收得掉 wb 那一条，另外两个方案原封不动。
        assert_eq!(s.purge_expired_drafts("wb", day).unwrap(), 1);
        assert_eq!(s.count_drafts("py").unwrap(), 1);
        assert_eq!(s.count_drafts("zr").unwrap(), 1);
        // 全表清一次把它们都收掉，且没碰 wb 那条还没过期的。
        // 第二个返回值是**剩余**条数——真机重定 `draft_max_entries` 就靠它。
        assert_eq!(s.purge_all_expired_drafts(day).unwrap(), (2, 1));
        assert_eq!(s.count_drafts("py").unwrap(), 0);
        assert_eq!(s.count_drafts("zr").unwrap(), 0);
        assert_eq!(
            s.search_drafts("wb", "aaaa", day).unwrap(),
            vec!["五笔新的"],
            "没过期的草稿不该被启动清理带走"
        );
        // ttl=0 ＝ 永不过期 ⇒ 不扫描、什么都不删（剩余数随之无意义，恒 0）
        assert_eq!(s.purge_all_expired_drafts(0).unwrap(), (0, 0));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn empty_batch_is_a_noop() {
        let p = tmp("wind_draft_empty.redb");
        let s = Store::open(&p).unwrap();
        assert_eq!(s.add_drafts("wb", &[], 0).unwrap(), 0);
        assert_eq!(s.count_drafts("wb").unwrap(), 0);
        let _ = std::fs::remove_file(&p);
    }

    /// 新草稿机制（P5 文本真源）端到端：`draft.dict.yaml` 里记的是**首次入库时间**，
    /// 折回、重开（按文本重建）之后仍然不续期、仍然是纯 FIFO。
    ///
    /// 这条刻意走 `Store` 门面 + 真实文本文件，而不是只测 `Runtime`：文本折回会丢掉
    /// `created_at`（`DRAFT` 表只有 `text/code/last_used` 三列），实现若哪天又改用
    /// `created_at` 判年龄，这里会直接红。
    #[test]
    fn draft_text_truth_keeps_first_seen_time_across_fold_and_reopen() {
        let p = tmp("wind_draft_text_truth.redb");
        let _ = std::fs::remove_dir_all(crate::store::derived_state_dir(&p));
        let _ = std::fs::remove_dir_all(crate::store::derived_text_dir(&p));
        let now = now_secs();
        let day = 86_400;
        let text_path = crate::store::derived_text_dir(&p).join("wb/draft.dict.yaml");

        {
            let s = Store::open(&p).unwrap();
            drafts_at(
                &s,
                &[
                    ("wb", "aaaa", "最老", now - 300),
                    ("wb", "bbbb", "中间", now - 200),
                    ("wb", "cccc", "最新", now - 100),
                ],
            );
            // 未过期 ⇒ 再写一次被跳过（不续期）
            assert_eq!(
                s.add_drafts("wb", &items(&[("aaaa", "最老")]), day)
                    .unwrap(),
                0
            );
            s.flush_user_data().unwrap();
            let text = std::fs::read_to_string(&text_path).unwrap();
            assert!(
                text.contains(&format!("最老\taaaa\t{}", now - 300)),
                "文本里该是首次入库时间（{}），实际:\n{text}",
                now - 300
            );
            // 纯 FIFO：最老的那条先走，且淘汰结果立刻折回文本
            assert_eq!(s.evict_drafts("wb", 2).unwrap(), 1);
            s.flush_user_data().unwrap();
            let text = std::fs::read_to_string(&text_path).unwrap();
            assert!(!text.contains("最老"), "淘汰结果应已写回文本:\n{text}");
        }

        // 模拟换机 / 状态目录丢失：只留文本，打开时按文本重建。
        std::fs::remove_dir_all(crate::store::derived_state_dir(&p)).unwrap();
        let s = Store::open(&p).unwrap();
        assert_eq!(s.search_drafts("wb", "bbbb", day).unwrap(), vec!["中间"]);
        assert_eq!(s.search_drafts("wb", "cccc", day).unwrap(), vec!["最新"]);
        assert!(
            s.search_drafts("wb", "aaaa", day).unwrap().is_empty(),
            "已淘汰的草稿不该从文本里复活"
        );
        // 重建之后仍不续期
        assert_eq!(
            s.add_drafts("wb", &items(&[("bbbb", "中间")]), day)
                .unwrap(),
            0
        );
        drop(s);
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_dir_all(crate::store::derived_state_dir(&p));
        let _ = std::fs::remove_dir_all(crate::store::derived_text_dir(&p));
    }

    /// 过期草稿重新打出 = 新草稿：文本里的时间戳被换掉，重开之后按新时间算年龄。
    #[test]
    fn an_expired_draft_gets_a_fresh_timestamp_in_the_text_file() {
        let p = tmp("wind_draft_text_expired.redb");
        let _ = std::fs::remove_dir_all(crate::store::derived_state_dir(&p));
        let _ = std::fs::remove_dir_all(crate::store::derived_text_dir(&p));
        let text_path = crate::store::derived_text_dir(&p).join("wb/draft.dict.yaml");
        let now = now_secs();
        let day = 86_400;
        let stale = now - 2 * day;
        {
            let s = Store::open(&p).unwrap();
            drafts_at(&s, &[("wb", "aaaa", "隔天的", stale)]);
            s.flush_user_data().unwrap();
            let text = std::fs::read_to_string(&text_path).unwrap();
            assert!(text.contains(&format!("隔天的\taaaa\t{stale}")), "{text}");

            // 过期 ⇒ 不该被当成「已存在」，这次真的落库并换上新时间戳
            assert_eq!(
                s.add_drafts("wb", &items(&[("aaaa", "隔天的")]), day)
                    .unwrap(),
                1,
                "过期的旧记录等同于不存在，这次该真的落库"
            );
            s.flush_user_data().unwrap();
            let text = std::fs::read_to_string(&text_path).unwrap();
            assert!(
                !text.contains(&format!("\taaaa\t{stale}")),
                "旧时间戳应被换掉:\n{text}"
            );
        }
        // 只留文本重开：新时间戳在文本里，重开后该能召回（不被当成过期）
        std::fs::remove_dir_all(crate::store::derived_state_dir(&p)).unwrap();
        let s = Store::open(&p).unwrap();
        assert_eq!(s.search_drafts("wb", "aaaa", day).unwrap(), vec!["隔天的"]);
        drop(s);
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_dir_all(crate::store::derived_state_dir(&p));
        let _ = std::fs::remove_dir_all(crate::store::derived_text_dir(&p));
    }
}
