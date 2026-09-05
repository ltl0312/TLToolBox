//! # 文本锚点插拔引擎（无损 · 幂等 · 行尾感知）
//!
//! TLToolBox 需要向用户 Shell 配置文件（如 PowerShell `$PROFILE`、bash `.bashrc`
//! 等）注入会话日志钩子。直接整文件覆写是危险的——配置文件里是用户的个人
//! 设置，任何截断 / 乱序 / 丢行都不可接受。本引擎为此提供**纯文本、零依赖**
//! 的锚点插拔原语：以带 TAG 的成对标记行圈定一个「TLToolBox 管理的区块」，
//! 注入 / 更新 / 移除都只触碰该区块内的行，文件其余字节原样保留。
//!
//! # 区块格式
//!
//! 一个区块 = 起始标记行 + 负载 + 结束标记行：
//!
//! ```text
//! # >>> TLToolBox <TAG> >>>
//! <PAYLOAD 任意多行>
//! # <<< TLToolBox <TAG> <<<
//! ```
//!
//! 其中 `<TAG>` 由调用方指定（建议 ASCII 标识符，如 `ps-readline-history`）。
//! 引擎按**整行精确匹配**（忽略行首尾空白）定位标记，不做子串 / 正则模糊匹配，
//! 因此与用户自己的 `# >>> ... >>>` 风格注释天然隔离。
//!
//! # 语义保证
//!
//! - **无损**：所有操作基于「按 `\n` 切分 → 局部改动 → 按 `\n` 重拼」的行模型，
//!   重拼在字节层面忠实复刻原文（含行内 `\r`），CRLF / LF 混合文件不会被改写；
//!   新增的块内行会**跟随宿主文件的行尾风格**（宿主含 `\r\n` 则块内行同样用
//!   CRLF），保证不把整文件搅成混合行尾。
//! - **幂等**：同 TAG 重复注入同一负载结果逐字节一致；重复注入新负载只更新
//!   既有区块，绝不叠放第二个同 TAG 区块（历史遗留的重复区块会被收敛为一个）。
//! - **可逆**：对同一内容先 `inject_block` 再 `remove_block`，逐字节还原注入前
//!   的原文——包括「文件末尾是否有换行」这类容易被忽略的细节（空内容视为空
//!   文件、处于行边界，追加后以换行收尾，移除后仍还原为空串）。
//! - **克制**：只删除 / 替换 TLToolBox 自己生成的标记行及其包裹内容。悬挂的
//!   起始标记（无配对结束标记，例如用户手删了结束行）不会被引擎吞并内容：
//!   [`inject_block`] 只把该行原位修复成完整区块，[`remove_block`] 则原样放过。
//!
//! # 边界约定
//!
//! - 空负载（或全空白负载）→ 区块内无负载行（仅起始 / 结束两行），仍合法；
//! - 同 TAG 同时存在「完整区块」与「悬挂起始标记」时，以完整区块为准；
//! - 负载中不应包含与标记同形的行（负载是调用方提供的可信内容，不做转义）。

/// 构造某 TAG 区块的**起始标记行**（`# >>> TLToolBox <TAG> >>>`）。
pub fn block_start_marker(tag: &str) -> String {
    format!("# >>> TLToolBox {tag} >>>")
}

/// 构造某 TAG 区块的**结束标记行**（`# <<< TLToolBox <TAG> <<<`）。
pub fn block_end_marker(tag: &str) -> String {
    format!("# <<< TLToolBox {tag} <<<")
}

/// 把负载文本切成「块内行」：剥掉首尾空白（含换行），按 `\n` 切分，并去掉
/// 每行可能携带的 `\r`（行尾风格统一交给 [`style_for_host`] 按宿主文件决定）。
/// 完全空白的负载 → 空向量（区块内无负载行）。
fn payload_lines(payload: &str) -> Vec<String> {
    let trimmed = payload.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    trimmed
        .split('\n')
        .map(|line| line.trim_end_matches('\r').to_string())
        .collect()
}

/// 组装一个完整区块的原始行序列：`[起始标记, ..负载行, 结束标记]`。
fn block_lines(tag: &str, payload: &str) -> Vec<String> {
    let mut lines = vec![block_start_marker(tag)];
    lines.extend(payload_lines(payload));
    lines.push(block_end_marker(tag));
    lines
}

/// 按宿主文件行尾风格给块内行补 `\r`：宿主文件含 `\r\n`（CRLF，Windows 上
/// 编辑器 / PowerShell 配置的常见形态）时，新增行也以 `\r` 结尾；否则保持纯
/// LF，避免把文件搅成混合行尾。
fn style_for_host(lines: Vec<String>, crlf: bool) -> Vec<String> {
    if !crlf {
        return lines;
    }
    lines
        .into_iter()
        .map(|mut line| {
            if !line.ends_with('\r') {
                line.push('\r');
            }
            line
        })
        .collect()
}

fn is_start_line(line: &str, tag: &str) -> bool {
    line.trim() == block_start_marker(tag)
}

fn is_end_line(line: &str, tag: &str) -> bool {
    line.trim() == block_end_marker(tag)
}

/// 从 `start_idx` 之后寻找同 TAG 的结束标记行；找不到 → `None`（悬挂起始）。
fn block_end_after(lines: &[String], start_idx: usize, tag: &str) -> Option<usize> {
    lines[start_idx + 1..]
        .iter()
        .position(|line| is_end_line(line, tag))
        .map(|offset| start_idx + 1 + offset)
}

/// 定位**第一个**完整区块 `(起始行下标, 结束行下标)`（均含标记行）。
///
/// 扫描时跳过悬挂的起始标记（它们不构成区块），继续向后找完整区块。
fn find_block(lines: &[String], tag: &str) -> Option<(usize, usize)> {
    (0..lines.len()).find_map(|i| {
        if is_start_line(&lines[i], tag) {
            block_end_after(lines, i, tag).map(|end| (i, end))
        } else {
            None
        }
    })
}

/// 把新区块追加到行序列末尾，同时忠实保留原文的「行边界」风格：
/// 原文以换行结尾（或为空内容——空文件本就处于行边界，追加结果以换行收尾）
/// → 结果同样以换行结尾；原文无结尾换行 → 结果同样不以换行结尾。保证后续
/// `remove_block` 能逐字节还原注入前内容。
fn append_block(mut lines: Vec<String>, block: Vec<String>) -> Vec<String> {
    // `split('\n')` 会在「以换行结尾」的内容尾部留下一枚空行幻影；先摘除它，
    // 追加区块后再按原样补回，使末尾换行语义不因追加而漂移。空内容切分出的
    // 单元素 `[""]` 同样按「行边界 + 末尾幻影」处理（结果以换行收尾）。
    let ends_with_newline = lines.is_empty() || lines.last().is_some_and(|line| line.is_empty());
    if let Some(last) = lines.last() {
        if last.is_empty() {
            lines.pop();
        }
    }
    lines.extend(block);
    if ends_with_newline {
        lines.push(String::new());
    }
    lines
}

/// 无损注入（或更新）一个 `TAG` 锚点区块，返回新内容。
///
/// 分派顺序：
/// 1. 已存在**完整** `TAG` 区块 → **更新**：整段替换首个区块的标记与内部内容，
///    同 TAG 的其余重复区块一并收敛移除（只触碰 TLToolBox 自己生成的行）；
/// 2. 不存在完整区块，但存在**悬挂起始标记** → 仅把该行原位替换为完整新区块
///    （修复残缺且不重复叠放）；
/// 3. 全新注入 → 把新区块**追加到文件末尾**。
///
/// 所有未被标记行圈中的内容（用户的注释、其他工具的锚点、其他 TAG 的
/// TLToolBox 区块……）一律逐字节保留。负载应传**不带首尾换行**的裸文本
/// （引擎会自行规范化并按宿主行尾风格落行）。
pub fn inject_block(content: &str, tag: &str, payload: &str) -> String {
    debug_assert!(!tag.is_empty(), "TAG 不能为空");

    let crlf = content.contains("\r\n");
    let block = style_for_host(block_lines(tag, payload), crlf);
    let lines: Vec<String> = content.split('\n').map(str::to_string).collect();

    // 1) 更新路径：整段替换首个完整区块，顺带收敛重复区块。
    if find_block(&lines, tag).is_some() {
        let mut out: Vec<String> = Vec::with_capacity(lines.len() + block.len());
        let mut replaced = false;
        let mut i = 0;
        while i < lines.len() {
            if is_start_line(&lines[i], tag) {
                if let Some(end) = block_end_after(&lines, i, tag) {
                    if !replaced {
                        out.extend(block.iter().cloned());
                        replaced = true;
                    }
                    i = end + 1; // 首个区块被替换；其余重复区块被丢弃
                    continue;
                }
                // 悬挂起始标记：不吞并其后的任何内容，原样保留。
            }
            out.push(lines[i].clone());
            i += 1;
        }
        debug_assert!(replaced, "已确认存在完整区块，必然完成一次替换");
        return out.join("\n");
    }

    // 2) 修复路径：无完整区块但存在悬挂起始标记 → 原位替换那一行。
    if let Some(dangling) = (0..lines.len()).find(|&i| is_start_line(&lines[i], tag)) {
        let mut out: Vec<String> = Vec::with_capacity(lines.len() + block.len());
        out.extend(lines[..dangling].iter().cloned());
        out.extend(block.iter().cloned());
        out.extend(lines[dangling + 1..].iter().cloned());
        return out.join("\n");
    }

    // 3) 全新注入路径：追加到文件末尾。
    append_block(lines, block).join("\n")
}

/// 无损移除内容中**所有** `TAG` 完整区块（起始 / 结束标记及其包裹内容），并
/// 清理移除动作自身制造的「多余空行」空洞后返回新内容。
///
/// 空行清理规则刻意**克制**：仅在移除点前后紧邻**均为**空行时删去其一，避免
/// 区块上下原有的分隔空行在移除后并成双空行空洞；更远的空行属于用户原有
/// 排版，一律保留。对未注入过该 TAG（或仅有悬挂标记）的内容，本函数是
/// **空操作**——逐字节原样返回。
pub fn remove_block(content: &str, tag: &str) -> String {
    debug_assert!(!tag.is_empty(), "TAG 不能为空");

    let mut lines: Vec<String> = content.split('\n').map(str::to_string).collect();

    // 循环移除全部完整区块（移除后下标整体前移，故每次重新定位）。
    while let Some((start, end)) = find_block(&lines, tag) {
        lines.drain(start..=end);
        // 局部空行收敛：仅当移除位置两侧紧邻均为空行时，删去其一。
        if start > 0
            && start < lines.len()
            && lines[start - 1].trim().is_empty()
            && lines[start].trim().is_empty()
        {
            lines.remove(start - 1);
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start(tag: &str) -> String {
        block_start_marker(tag)
    }

    fn end(tag: &str) -> String {
        block_end_marker(tag)
    }

    /// 统计内容中 TAG 起始标记的个数（用于验证不重复叠放 / 不误伤其他 TAG）。
    fn count_blocks(content: &str, tag: &str) -> usize {
        content.matches(&start(tag)).count()
    }

    // ---- 场景一：全新插入 ----

    #[test]
    fn inject_into_empty_content_creates_block() {
        // 空内容（空文件）→ 区块即全文；空文件处于行边界，结果以换行收尾。
        let out = inject_block("", "history", "payload-a\npayload-b");
        assert_eq!(
            out,
            "# >>> TLToolBox history >>>\npayload-a\npayload-b\n# <<< TLToolBox history <<<\n"
        );
        assert_eq!(count_blocks(&out, "history"), 1);
        // 移除后逐字节还原为空串。
        assert_eq!(remove_block(&out, "history"), "");
    }

    #[test]
    fn inject_appends_to_end_and_preserves_user_content() {
        let original = "# 用户自己的 PowerShell 配置\n$ErrorActionPreference = 'Stop'\n";
        let out = inject_block(
            original,
            "history",
            "Set-PSReadLineOption -HistorySavePath X",
        );
        // 原内容逐字节保留在区块之前（作为前缀）。
        assert!(out.starts_with(original), "用户内容应完整保留: {out}");
        // 区块追加在末尾，格式与注入规格逐字一致。
        let expected = format!(
            "{original}{}\nSet-PSReadLineOption -HistorySavePath X\n{}\n",
            start("history"),
            end("history")
        );
        assert_eq!(out, expected);
        assert_eq!(count_blocks(&out, "history"), 1);
    }

    // ---- 场景二：重复插入（更新）----

    #[test]
    fn inject_update_replaces_payload_in_place() {
        let original = "user-a\n# >>> TLToolBox history >>>\nold-payload\n# <<< TLToolBox history <<<\nuser-b\n";
        let out = inject_block(original, "history", "new-payload");
        let expected =
            "user-a\n# >>> TLToolBox history >>>\nnew-payload\n# <<< TLToolBox history <<<\nuser-b\n";
        assert_eq!(out, expected, "应原位替换内部内容且不惊动前后用户行");
        assert!(!out.contains("old-payload"), "旧负载应被彻底替换");
        assert_eq!(count_blocks(&out, "history"), 1);
    }

    #[test]
    fn inject_update_is_idempotent() {
        let once = inject_block("x\ny\n", "history", "p1");
        let twice = inject_block(&once, "history", "p1");
        assert_eq!(once, twice, "同负载重复注入应逐字节一致");
        assert_eq!(count_blocks(&twice, "history"), 1);
    }

    #[test]
    fn inject_converges_duplicate_blocks_to_one() {
        // 历史遗留的重复区块（同 TAG 两个完整块）→ 更新后收敛为单一区块。
        let dup = format!(
            "{}\na\n{}\n{}\nb\n{}\n",
            start("history"),
            end("history"),
            start("history"),
            end("history")
        );
        let out = inject_block(&dup, "history", "new");
        assert_eq!(count_blocks(&out, "history"), 1, "重复区块应被收敛");
        assert_eq!(
            out,
            format!("{}\nnew\n{}\n", start("history"), end("history")),
            "首个区块被替换、其余重复区块被移除"
        );
    }

    // ---- 场景三：彻底移除 ----

    #[test]
    fn remove_restores_original_exactly_after_inject() {
        // 包含注释、空行、他工具锚点、其他 TAG 区块的复杂原文（CRLF）。
        let original = concat!(
            "# 用户自己的配置（CRLF 行尾）\r\n",
            "<# 多行块注释 #>\r\n",
            "\r\n",
            "# >>> OhMyPosh >>>\r\n",
            "oh-my-posh init pwsh\r\n",
            "# <<< OhMyPosh <<<\r\n",
            "\r\n",
            "# >>> TLToolBox autostart >>>\r\n",
            "Start-Process -FilePath 'D:\\toolbox.exe'\r\n",
            "# <<< TLToolBox autostart <<<\r\n",
        );
        let injected = inject_block(original, "history", "hook-line-1\nhook-line-2");
        assert_ne!(injected, original, "注入后内容应确实发生变化");
        assert_eq!(count_blocks(&injected, "history"), 1);

        let removed = remove_block(&injected, "history");
        assert_eq!(
            removed, original,
            "注入后原样移除应逐字节还原原文（含 CRLF 与其余区块）"
        );
    }

    #[test]
    fn remove_without_block_is_noop() {
        // 从未注入过该 TAG → 空操作，逐字节返回（含 CRLF / 末尾多空行）。
        let content = "# 干干净净的用户配置\r\nabc\r\n\r\n";
        assert_eq!(remove_block(content, "ghost"), content);
    }

    #[test]
    fn remove_cleans_the_double_blank_hole_it_would_otherwise_leave() {
        // 区块上下各有一行分隔空行：移除后若不清扫会并成双空行空洞，
        // 引擎应删去其一，保留单空行分隔。
        let content = format!("a\n\n{}\nold\n{}\n\nb\n", start("h"), end("h"));
        let out = remove_block(&content, "h");
        assert_eq!(out, "a\n\nb\n", "应清理移除产生的多余空行");
        assert_eq!(count_blocks(&out, "h"), 0);
    }

    #[test]
    fn remove_no_trailing_newline_content_roundtrips_exactly() {
        // 文件末尾没有换行是「无损」最容易翻车的细节：注入再移除必须还原。
        let original = "line-without-trailing-newline";
        let injected = inject_block(original, "h", "p");
        assert!(
            injected.ends_with(&end("h")),
            "原文无结尾换行 → 注入结果也不应以换行收尾"
        );
        assert_eq!(remove_block(&injected, "h"), original);
    }

    // ---- 场景四：原有用户配置无损保留（含行尾风格）----

    #[test]
    fn crlf_host_file_keeps_crlf_through_update_and_remove() {
        let original = "x\r\n# >>> TLToolBox h >>>\r\nold\r\n# <<< TLToolBox h <<<\r\ny\r\n";
        // 更新：块内新增行必须跟随宿主的 CRLF 风格，不得把文件搅成混合行尾。
        let updated = inject_block(original, "h", "new1\nnew2");
        let expected =
            "x\r\n# >>> TLToolBox h >>>\r\nnew1\r\nnew2\r\n# <<< TLToolBox h <<<\r\ny\r\n";
        assert_eq!(updated, expected, "CRLF 宿主下块内行应同样使用 CRLF");
        // 移除后与「对原始 CRLF 内容直接移除」结果一致，行尾风格依旧不漂移。
        assert_eq!(
            remove_block(&updated, "h"),
            remove_block(original, "h"),
            "更新后移除应还原为不含区块的 CRLF 内容"
        );
        assert_eq!(remove_block(&updated, "h"), "x\r\ny\r\n");
    }

    #[test]
    fn payload_leading_trailing_newlines_are_normalized() {
        // 调用方随手多传首尾换行 / 空白 → 引擎应规范化为干净区块。
        let with_junk = inject_block("base\n", "h", "\n  hook-a\nhook-b\n\n");
        let clean = inject_block("base\n", "h", "hook-a\nhook-b");
        assert_eq!(with_junk, clean, "负载首尾空白应被剥除");
    }

    #[test]
    fn other_tags_and_dangling_markers_are_left_alone() {
        // 他 TAG 的 TLToolBox 区块 + 悬挂起始标记（无结束行）：移除本 TAG 时
        // 两者都必须原样保留，不得被吞并或误删。
        let content = format!(
            "keep-me\n{}\nautostart-body\n{}\n{}\nstray-content-after-dangling\n",
            start("autostart"),
            end("autostart"),
            start("history") // 悬挂起始：没有配对的结束标记
        );
        let out = remove_block(&content, "history");
        assert_eq!(out, content, "无完整区块时移除应为空操作");
        assert_eq!(count_blocks(&out, "autostart"), 1);

        // 注入则只把悬挂行原位修复成完整区块，不重复叠放、不吞并后续内容。
        let repaired = inject_block(&content, "history", "hook");
        assert_eq!(count_blocks(&repaired, "history"), 1);
        assert_eq!(count_blocks(&repaired, "autostart"), 1);
        assert!(
            repaired.contains("stray-content-after-dangling"),
            "悬挂标记之后的内容不得被吞并"
        );
        assert!(repaired.contains("keep-me"), "其余用户内容不得丢失");
    }
}
