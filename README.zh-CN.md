# Jev CLI

[English](README.md)

一个无状态的类型化语义判断 CLI，调用 Jev 模型（TypeSafe System One），附带一个把判断能力引入脚本、CI 与 agent 工作流的 Agent Skill。Jev 不生成文本：你提供 state 与类型化问题，它返回校准过的概率与类型化答案。CLI 把判断变成 shell 原语——stdout 是稳定 JSON，stderr 是诊断信息，退出码可以直接分支。

CLI 固定使用模型 `jev-latest`。

## 安装

Skill 与 CLI 分开安装。

```sh
npx skills add model-clis/jev
```

Windows（推荐）：

```powershell
scoop bucket add model-clis https://github.com/model-clis/homebrew-packages
scoop install model-clis/jev
```

无 Scoop 的 Windows：

```powershell
irm https://raw.githubusercontent.com/model-clis/jev/main/scripts/install.ps1 | iex
```

Apple Silicon macOS（推荐）：

```sh
brew tap model-clis/packages
brew install jev
```

Linux x64 或无 Homebrew 的 macOS Apple Silicon：

```sh
curl -fsSL https://raw.githubusercontent.com/model-clis/jev/main/scripts/install.sh | sh
```

安装脚本只从 GitHub Release 下载资产、校验 SHA-256，默认安装到 `~/.local/bin`。可用 `JEV_INSTALL_DIR` 修改目录，或用完整 tag（如 `JEV_VERSION=v2026.918.0`）固定版本。显式执行安装脚本可以覆盖已有二进制；skill 绝不静默安装或升级。

## 登录与使用

在你自己的终端私密配置 TypeSafe API key——不要粘贴到聊天里：

```sh
jev login
```

单问快捷命令（答案 id 固定为 `q`；state 来自 `--state` 或 stdin）：

```sh
echo "$LOG" | jev noul "这段堆栈是否表明发生了 OOM?" \
    --true "OOM killer 或无法分配内存" --false "其他任何故障"

jev choice "这个 issue 应该由哪个团队处理?" \
    --opt billing="支付、账单、退款" \
    --opt technical="缺陷、故障、集成" \
    --opt sales="定价、升级、新账户" \
    --state @ticket.json

jev score "这份文档有多新?" \
    --level "与 API 完全一致" --level "轻微过时" --level "严重过时" \
    --state @README.md
```

完整请求（state + questions）来自文件、stdin 或预设：

```sh
jev ask request.json
cat request.json | jev ask -
jev ask --preset triage --param issue=@issue.json --param repo=acme/api
```

`@PATH` 参数会保持 JSON 文件的结构，其他文件按纯文本处理。

## 断言：把答案变成退出码

所有判断命令都接受 `--assert EXPR`。表达式对响应封套（`model`、`answers`、`usage`）求值；为假时 stdout 仍输出完整 JSON、退出码为 `3`。无法求值（路径不存在、类型不匹配）时退出码为 `1`——坏掉的门禁绝不能静默放行。

```text
expr       := or
or         := and { "or" and }
and        := unary { "and" unary }
unary      := "not" unary | "(" expr ")" | comparison
comparison := operand op operand | operand "in" list
operand    := path | number | true | false | "string"
path       := ident { "." ident | "[" "string" "]" }
op         := <= | >= | == | != | < | >
list       := "[" operand { "," operand } "]"
```

```sh
jev ask --preset review-gate --param diff=@pr.diff \
    --assert 'answers.destructive.noul <= 0.5 and answers.touches_creds.noul <= 0.5'

echo "$LOG" | jev noul "OOM?" --assert 'answers.q.noul >= 0.8'
jev ask --preset triage --assert 'answers.area.choice in ["billing", "technical"]'
jev ask --preset risk --assert 'answers.risk.probabilities["needs human review"] >= 0.25'
```

字符串用双引号，整个表达式用单引号包裹。`not` 结合最紧，其次是 `and`，再次是 `or`。不支持算术；需要加权组合时用 `jq` 处理输出。

退出码：`0` 成功、`1` 使用或基础设施错误（含断言无法求值）、`2` 批量部分成功、`3` 断言为假、`130` 中断。`0` 不代表"答案为是"——请读取 `answers` 或使用 `--assert`。

## 预设

预设是从 `./.jev/presets/*.json`（仓库级，重名时优先）与用户配置目录（Linux 为 `~/.config/jev/presets/`）发现的请求模板。提交进仓库后，问法与阈值就成为可 review 的自动化资产。

```json
{
  "description": "Issue 分诊：严重度、路由、是否无效",
  "params": {"issue": "issue 正文", "repo": "仓库名"},
  "state": {"issue": "{{issue}}", "repo": "{{repo}}"},
  "questions": {
    "severity": {"type": "choice", "instructions": "这个 issue 有多严重?", "criteria": {"blocker": "数据丢失或服务不可用", "major": "功能损坏但有规避办法", "minor": "外观或体验问题"}},
    "invalid": {"type": "noul", "instructions": "这份报告是否无效?", "criteria": {"true": "灌水、重复或无法复现", "false": "真实报告"}}
  },
  "assert": "answers.invalid.noul <= 0.5"
}
```

`description`、`params`、`assert` 是保留字段，发送前剥离；`assert` 提供默认断言，显式 `--assert` 会整体替换。`{{name}}` 占位符必须填满整个字段值，从 `--param NAME=VALUE`、`--param NAME=@PATH`（JSON 按子树注入）或 `--param NAME=@-`（stdin）取值。缺少参数会报错并指明占位符位置；多余参数给出警告。

`jev presets list`、`jev presets show NAME`、`jev presets validate [NAME|PATH]` 全部离线工作——validate 在花费 token 之前拦截畸形问题与断言语法错误。

## 批量

`jev map` 用一个模板并发评估大量输入，按完成顺序流式输出 JSONL，每行带输入 `index`：

```sh
jev map --preset triage --in issues.jsonl --out triaged.jsonl --concurrency 8
jev map --preset classify --in lines.txt --each text
```

- `--each state`（默认）：每行是任意 JSON 值，作为该请求的 state。
- `--each text`：每行按纯文本字符串作为 state。
- `--each request`：每行是完整请求体——问题需要逐行变化时用 `jq` 生成。

单行失败只影响那一行（`{"index":N,"ok":false,"error":{"kind":"api"|"network"|"usage","message":"..."}}`），整批退出码为 `2`。断言逐行生效，任一行为假退出 `3`（行失败仍优先给 `2`）。配合 `--out FILE --resume`，重跑时跳过已完成 `ok` 的行。这是对你自己输出文件的断点续跑，不是缓存——CLI 从不记忆化结果。

重试与限额：429/529 与网络错误按指数退避（500ms 起步、最多 5 次、尊重 `retry_after_ms`），单请求 30 秒超时，`--concurrency` 默认 4（官方约 20 请求/秒；建议不超过 8）。

## 输出合同

stdout 永远是稳定的单行 JSON（`map` 每行一个 JSON 对象）；stderr 承载进度、用量汇总与可操作的错误。`--capture-diagnostics` 把 CLI 诊断写入安全临时文件，仅在退出码不是 `0`、`2`、`3` 时保留，并把 `JEV_DIAGNOSTICS=<path>` 作为 stderr 最后一行输出。

## 开发

要求 Rust 1.89 或更高。

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets --all-features
npx skills add . --list
sh -n scripts/install.sh
```

## 发布

自动化每日 workflow 仅当 `main` 有最新 release 未包含的提交时发布。版本使用香港日期（`vYYYY.MDD.REV`）。为 Windows x64、Linux x64（musl）、macOS Apple Silicon 产出稳定资产，各带 `.sha256` 文件。没有 nightly 或预发布渠道。

Homebrew 与 Scoop 元数据维护在 [`model-clis/homebrew-packages`](https://github.com/model-clis/homebrew-packages)，每日与最新 release 对账。

仓库：<https://github.com/model-clis/jev>

## 许可

MIT
