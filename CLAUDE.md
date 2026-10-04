# agentrun

## 目标

agentrun 是一个独立的 Rust 命令行工具，在本机 macOS、本机 Linux、Cloud Managed 容器和 docker 容器里安全地运行 claude-code、codex 和 pi。调用方在每种环境里的用法相同，工具在这些环境里能安装，出了问题能查出原因。

- Safety：在每个环境能提供的范围内隔离 agent，限制它能写的文件、执行的命令能否联网、能读到的配置和账号。
- Unified Experience：同一条命令在四种环境里都能运行，输出格式与成败判定相同。
- Doctor：每个平台一个可执行文件，能在启动前检查环境，失败时能看出原因。

设计从 `docs/index.html` 开始读，Task 计划与进度见 GitHub issue #1。

## 协作方式

用户与 AI 平等协作。对 AI 的要求：

- 主动提出能改进工具的想法。
- 发现方向、方案或文档有问题，直接指出并给出理由。
- 被质疑时认真回应，不为文档或已有代码辩护。文档记录的是写下时的共识，可以修改。遇到按文档做不下去的情况，停下来报告冲突并给出替代方案，由用户当场决定是否修改；决定修改后，文档与代码一起改。
- 报告为完成的工作必须已经验证过。没做的、跳过的、不确定的，在报告里写明。

## 工作流程

整体流程是 Design -> Graph[Task(Spec -> Implement -> Test -> Validate)]：先写 Design，再拆出多个 Task，Task 之间可以有依赖；每个 Task 依次经过 Spec、Implement、Test、Validate。

### Design

Design 使用 HTML 文档，主要由用户描述需求和预期，放在 `docs/`，具体内容由用户和 AI 一起维护，可以加可视化内容来描述需求。文档的格式要求见「文档规范」。

### Task

一个 Task 对应 Design 中的一个模块或功能，也对应一个 GitHub issue。每一步的执行者和是否需要用户确认如下：

| 步骤 | 执行者 | 用户确认 |
|---|---|---|
| 创建 Task issue（label `task`） | 主 agent | 由用户提出 |
| 与用户沟通需求并写 Spec | 主 agent | 用户审核通过后才开始开发 |
| Implement 和 Test | implementer subagent | 不需要 |
| Review | 主 agent | 不需要；同一个 subagent 两轮未通过时报告用户 |
| 偏离 Design Doc、新增 Design Doc 没有的字段或选项、Design Doc 没给数值的常量 | 主 agent 提出 | 需要 |
| 在 issue 中写 comment | 主 agent | 不需要 |
| Validate | 用户 | AI 提供测试步骤或临时脚本 |
| commit、PR、关闭 issue | 主 agent | 需要 |

补充规则：

- Spec 描述实际的实现细节，要做到自包含，让 subagent 不需要知道其他背景就可以执行；如果需要其他背景，在 Spec 中引用。
- 实现过程中发现 Spec 有误，在 issue comment 中修正。
- Task 之间会形成依赖链，也会发现之前的 Task 有错，这都是正常的，在新 Task 的 comment 中记录。
- Implement 和 Test 的具体规则见 `.claude/agents/implementer.md`。普通任务使用 implementer 默认的 opus 模型；复杂任务在调用时指定 `model: fable`；更复杂的任务可以让多个 fable 和 opus subagent 协作。
- Review 的依据是本文件和 `.claude/agents/implementer.md`。改动涉及 `docs/` 时，主 agent 要看 `scripts/check_design.py` 生成的截图。退回时指出未通过的条目。

### 小任务

经用户明确同意，小任务可以跳过上面的流程，也可以把多个小任务合并到一个 PR。

## 代码规范

- 代码里不写注释和 docstring，代码的含义靠命名与结构表达。唯一例外是代码看起来可以删除或修改、实际上不能改的地方，写一行 `why(#N): 原因`，必须带 issue 编号，尽量少用。工具指令（如 `# noqa`、`#pragma`）不算注释。检查由 `scripts/check_comment.py` 执行，该脚本在项目确定语言后再写，现在是空文件。
- commit message 的格式是 `<type>(<scope>): <中文一句话>`。
- PR 标题用一句话说明改了什么。PR 描述分两节，末尾写 `Closes #N`：
  - `## Summary`：每项一行，写改了哪个文件或模块、它做什么。设计文档只写增加或修改了哪个页面。
  - `## Main Takeaway`：这个 PR 带来了什么、有什么变化，用读者能直接看到的形式给出。命令行行为贴命令与实际输出，界面改动贴截图。按 PR 内容选择写法，例如改动设计时给出设计中的预期输出，实现功能时给出当前能运行的命令与输出，完善已有功能时给出前后对比。
  - 背景、决定、验证过程与环境问题写在 issue 中，不写进 PR。

## 文档规范

### Design Doc

写以下内容：

- 用户能看到的行为：命令、选项、配置格式等；
- 重要模块的输入输出、API 接口类定义；
- 影响结果的常量，例如阈值、超时、上限、默认值等；
- 改动时容易被破坏的设计决定，附一句理由。

以下内容不写：

- 代码里能直接读到的实现步骤、正则、规则名单的具体条目；
- 修改历史、实测过程、调研依据，这些写在 issue 与 commit message。

章节按读者会问的问题划分。上面列的几类内容写在相关章节里，和它们解释的机制放在一起，不单独成章；常量不在文末另做汇总。

文件组织：

- `docs/index.html` 是总览页，按 Motivation、Environment、Event and Output、Runtime、TBD、暂不做的功能六节列出所有文档。
- `docs/style.css` 是所有页面共用的样式。新文档参考 `pages/isolation.html` 的写法，保留 `<head>`（字体、MathJax、Prism 的引入），按需使用 style.css 中的组件。
- 项目的设计文档放在 `docs/pages/`。
- `docs/` 由 GitHub Pages 从 `main` 分支的 `/docs` 目录发布到 https://yhnocoder.github.io/agentrun/ ，合并到 `main` 后自动更新。`docs/` 下的所有文件都会公开，`docs/.nojekyll` 让 Pages 原样发布文件。
- 改动 `docs/` 后运行 `uv run scripts/check_design.py`（默认用本机的 Chrome；在 Cloud Managed 容器里加 `--browser chromium`，用 playwright 自带的 Chromium），它对每个页面截取桌面、375px、深色三种截图，并报告控制台错误、资源加载失败、公式渲染错误、窄屏横向溢出和断开的相对链接。脚本只能发现机械性错误，布局是否符合设计仍然需要看截图。

### Intro Doc

除了 Design Doc 外，还需要提供给用户看的 Intro Doc。Intro Doc 同样采用 HTML，共享 Design Doc 的样式代码，在 v1.0 之前完成，开始时间由用户指定。

## 行文

平实：

1. 不用比喻或拟人来命名、解释机制，直接写它是什么、做什么。
2. 不用口语衬词和轻佻语气，写动作本身。例如写「同时写出」，不写「顺手落一份」；写「检查环境」，不写「探一眼」。
3. 不过度缩写，不自造缩写，不省略主语，不为了省字写压缩句。把意思写完整。
4. 不用「不是 X，而是 Y」的对照句、破折号、排比和口号式的句子。

文档、README、Artifact 和 GitHub issue 都按以上规则写。

## 调研记录

调研外部项目、方案或工具时：

1. 报告正文发布成 Artifact（claude.ai 上的私有页面），不写进仓库。之后的 agent 会搜索和读取仓库里的文件，调研过程写进仓库会混进这些结果。
2. 同时开一个 issue，加 `question` 标签，写明调研动机、Artifact 链接与结论摘要。结论确定后关闭 issue。
3. 调研结论中需要实施的改动，写进 Design Doc 或 feature issue，注明来源 issue 编号。
