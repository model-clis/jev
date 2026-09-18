# 注意

## 项目约束

- 本项目是无状态 Jev（TypeSafe System One）判断 CLI，不是权限沙箱，也不做任何结果缓存。每次调用都真实请求 API；记忆化、去重等需求由调用方组合其他工具实现。
- 使用 `Cargo.toml` 声明的最低 Rust 版本。调整最低版本或依赖后，同步更新文档、`Cargo.lock`、CI 和 release workflow；可复现的构建与测试使用 `--locked`。
- 修改 Rust 代码后至少运行：
  - `cargo +1.89.0 fmt --all -- --check`
  - `cargo +1.89.0 clippy --locked --all-targets --all-features -- -D warnings`
  - `cargo +1.89.0 test --locked --all-targets --all-features`
- `model-clis/homebrew-packages` 是本项目的下游包元数据仓库，维护 Homebrew Formula 和 Scoop manifest。发布新版本，或修改 release 资产名称、校验和及安装方式后，应在 release 完成后运行该仓库的 `Update packages` workflow，并确认生成文件校验与提交步骤成功。
- 保持工具接口小而明确。stdout 永远是稳定、结构化且有界的 JSON/JSONL；stderr 是进度与诊断；错误应包含足够的上下文和可执行的恢复提示。区分工具基础设施失败（`1`）、批量部分成功（`2`）与判断结果本身（`0`/`3`）。
- 断言无法求值（路径缺失、类型不匹配）必须报错退出 `1`，绝不能静默按 false 处理；门禁场景里静默 false 等于悄悄放行。
- `jev map` 的 `--resume` 只是读取用户自己指定的 `--out` 文件并跳过已完成行，不是缓存，也不引入任何跨调用隐藏状态。

## 判断与接口行为

- 请求构造（state/questions/criteria 形状、占位符替换、保留字段剥离）、断言语义和输出合同是本项目的核心接口；修改它们必须在 `tests/cli.rs` 补端到端用例（wiremock 模拟 `/v1/systemone`），覆盖退出码、stdout 纯净性和错误信息可操作性。
- README 与 `skills/using-jev/SKILL.md` 中的示例必须与实现严格一致，改动接口时同步更新；skill 描述保持简洁、中性，只描述能力、参数和结果语义。
- 对会影响模型判断质量的问题措辞或阈值示例改动，应先在真实数据上验证再定稿；文档中的阈值是示例，不是通用规则。
- 涉及真实 API 行为（认证、限流、错误体格式）的改动，如有条件应使用真实 key 做一次行为冒烟，并把观察到的契约固化进 wiremock 测试。
