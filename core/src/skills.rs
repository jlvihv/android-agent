//! Skill：一个目录 = 一份 `SKILL.md`（开头的 frontmatter 写名字和一句描述，正文写做法）
//! + 它要用的脚本 / 资料。
//!
//! **不开工具、也不加 `agent` 子命令**：用一个 skill 就是 `read` 它的 `SKILL.md`、
//! 用 bash 跑里面的脚本 —— 模型现有的两个门就够了，再包一层只会让「看到的就是文件
//! 本身」变成看到一层转述。
//!
//! 要做的是让模型**知道有哪些**：skill 靠「描述被看见」来触发，所以系统提示词里放一份
//! 索引（名字 + 一句描述 + 路径，见 [`index`]），正文留给模型按需去读。只给一个「去查
//! 有哪些 skill」的命令的话，模型不知道什么时候该查。
//!
//! **怎么装 / 怎么写 skill 也是一个 skill**（内置的 [`CREATE_SKILL`]）：skill 一般是用户
//! 让 agent 装的，而不是自己往目录里放，所以模型得知道目录在哪、`SKILL.md` 长什么样。
//! 这些写进提示词的话每轮都带着；做成一个 skill，索引里只占一行，用到时才 read。
//! 核心每次启动把它写进 skill 目录（内容没变就不动，被删了会补回来）。
//!
//! 索引是动态的（装一个 skill 提示词就变），拼在冻结的那份提示词后面（见
//! `AppState::system_prompt`）：skill 不变时每个会话的前缀逐字节相同、照常命中缓存；
//! 变了的话 `ensure_system_prompt` 会把新版本追加进老会话，下一轮全价一次。所以索引
//! 要**短**、要**稳**（按目录名排序，同样的目录永远拼出同样的字节）。
//!
//! 放在哪：手机上是 `/workspace/skills`（挂了存储就是内部存储的 Agent 文件夹，用户在
//! 文件管理器里看得到、能自己往里放），电脑上是我们的数据目录下的 `skills`。见
//! `LinuxSandbox::skills_dir`。装一个 skill 就是往里放一个目录（模型用 bash 就能
//! `git clone`）。

use std::path::Path;

/// 索引里最多列多少个（再多就该整理了，提示词不能无限长）。
const MAX_SKILLS: usize = 50;
/// 一句描述最多多少字（超了截断加 `…`）。
const MAX_DESCRIPTION: usize = 300;

/// 一个装好的 skill。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// `SKILL.md` 在模型视角里的路径（它 `read` 的就是这个）
    pub path: String,
}

/// 扫一遍 skill 目录。`host` 是磁盘上的目录，`guest` 是模型看到的同一个目录。
///
/// 每个子目录里有 `SKILL.md`、frontmatter 里有 `description` 的才算；没有名字就用目录名。
/// 按目录名排序（索引要逐字节稳定）。目录不存在 = 一个都没有。
pub fn scan(host: &Path, guest: &str) -> Vec<Skill> {
    let Ok(rd) = std::fs::read_dir(host) else {
        return Vec::new();
    };
    let mut dirs: Vec<String> = rd
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with('.'))
        .collect();
    dirs.sort();

    let mut out = Vec::new();
    for dir in dirs {
        let Ok(text) = std::fs::read_to_string(host.join(&dir).join("SKILL.md")) else {
            continue;
        };
        let Some(meta) = frontmatter(&text) else {
            tracing::debug!("skill {dir}：SKILL.md 没有 frontmatter，跳过");
            continue;
        };
        let Some(description) = meta.description.filter(|d| !d.is_empty()) else {
            tracing::debug!("skill {dir}：没写 description，跳过");
            continue;
        };
        out.push(Skill {
            name: meta.name.filter(|n| !n.is_empty()).unwrap_or_else(|| dir.clone()),
            description: clip(&description, MAX_DESCRIPTION),
            path: format!("{}/{dir}/SKILL.md", guest.trim_end_matches('/')),
        });
        if out.len() >= MAX_SKILLS {
            break;
        }
    }
    out
}

/// 拼进系统提示词的那一段。一个都没有就是空串。
pub fn index(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut s =
        String::from("- 技能（skill）：遇到下面这些事，先用 read 读对应的 SKILL.md，按里面的做法来：");
    for k in skills {
        s.push_str(&format!("\n  - {}：{}（{}）", k.name, k.description, k.path));
    }
    s
}

/// 内置的那个 skill 的目录名：教模型怎么新建 / 安装 / 修改 skill。
pub const CREATE_SKILL: &str = "create-skill";

/// `create-skill/SKILL.md` 的内容。`dir` 是 skill 目录（模型视角），按机器填进去。
pub fn create_skill_md(dir: &str) -> String {
    let dir = dir.trim_end_matches('/');
    format!(
        r#"---
name: create-skill
description: 用户让你新建、安装（比如从 git 仓库装）、修改或删除技能（skill）时先读这个：skill 放在哪、SKILL.md 怎么写、怎么装。
---
# 新建 / 安装 / 修改 skill

skill 放在 `{dir}/`，一个 skill 一个目录：

```
{dir}/<名字>/
  SKILL.md      必须有
  scripts/…     可选：要跑的脚本
  …             可选：模板、参考资料
```

## SKILL.md 怎么写

开头两行 `---` 之间是 frontmatter，只认两个字段：

```
---
name: <名字，小写字母 / 数字 / ->
description: <一句话写清「什么时候该用它」>
---
```

- **description 最要紧**：它会进系统提示词的技能列表（最多 300 字），你以后会不会想起
  用这个 skill 全看它。写「遇到什么事时用」，别只写「这是什么」。
- 下面是正文：步骤、注意事项、要跑的命令。正文不进提示词，用到时才 read，可以写详细。
- 脚本放进同一个目录，正文里用完整路径写（`{dir}/<名字>/scripts/x.sh`）。

## 安装别人写好的

- 仓库本身就是一个 skill（根目录有 SKILL.md）：`git clone <地址> {dir}/<名字>`
- 仓库里有好几个 skill（常见是 `skills/<名字>/SKILL.md`）：先 clone 到 /tmp，再把要的
  那几个目录拷进 `{dir}/`
- **原样拷进来，不要改它的文件**（frontmatter 里多出来的字段比如 license 不碍事，
  只认 name / description）；用户要你改才改
- 装好检查：`{dir}/<名字>/SKILL.md` 在**第一层**，frontmatter 里有 description

## 生效

放进去之后，**下一轮**就会出现在系统提示词的技能列表里。没出现的话：SKILL.md 不在目录
第一层，或者 frontmatter 没写 description。

## 修改 / 删除

直接改 SKILL.md；删除就删掉整个目录。（这个 create-skill 是内置的，删了下次启动会补回来。）
"#
    )
}

#[derive(Debug, Default, PartialEq)]
struct Meta {
    name: Option<String>,
    description: Option<String>,
}

/// `---` 包着的那段 YAML 里取 `name` / `description`。
///
/// 只认这两个键，也只认 skill 实际会写的几种形状：单行（可带引号）、
/// `>` / `|` 折叠块（下面缩进的几行拼成一句）。不引一个 YAML 库进来。
fn frontmatter(text: &str) -> Option<Meta> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.lines();
    if lines.next()?.trim_end() != "---" {
        return None;
    }
    let body: Vec<&str> = lines.take_while(|l| l.trim_end() != "---").collect();

    let mut meta = Meta::default();
    let mut i = 0;
    while i < body.len() {
        let line = body[i];
        i += 1;
        if line.starts_with([' ', '\t']) {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        let value = if matches!(value, ">" | "|" | ">-" | "|-" | ">+" | "|+") {
            // 折叠块：下面缩进的那几行
            let mut parts = Vec::new();
            while i < body.len() && (body[i].starts_with([' ', '\t']) || body[i].trim().is_empty()) {
                let t = body[i].trim();
                if !t.is_empty() {
                    parts.push(t);
                }
                i += 1;
            }
            parts.join(" ")
        } else {
            unquote(value).to_string()
        };
        match key.trim() {
            "name" => meta.name = Some(value),
            "description" => meta.description = Some(value),
            _ => {}
        }
    }
    Some(meta)
}

fn unquote(s: &str) -> &str {
    for q in ['"', '\''] {
        if let Some(inner) = s.strip_prefix(q).and_then(|r| r.strip_suffix(q)) {
            return inner;
        }
    }
    s
}

fn clip(s: &str, max: usize) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() <= max {
        return s;
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("agent-skills-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn put(root: &Path, dir: &str, body: &str) {
        std::fs::create_dir_all(root.join(dir)).unwrap();
        std::fs::write(root.join(dir).join("SKILL.md"), body).unwrap();
    }

    #[test]
    fn frontmatter_shapes_skills_actually_use() {
        let m = frontmatter("---\nname: pdf\ndescription: 处理 PDF\n---\n# 正文\n").unwrap();
        assert_eq!(m.name.as_deref(), Some("pdf"));
        assert_eq!(m.description.as_deref(), Some("处理 PDF"));

        let m = frontmatter("---\nname: \"x\"\ndescription: 'a: b'\nlicense: MIT\n---\n").unwrap();
        assert_eq!(m.name.as_deref(), Some("x"));
        assert_eq!(m.description.as_deref(), Some("a: b"), "值里的冒号不能截断");

        let m = frontmatter("---\nname: y\ndescription: >\n  第一行\n  第二行\n\nother: 1\n---\n").unwrap();
        assert_eq!(m.description.as_deref(), Some("第一行 第二行"));

        assert_eq!(frontmatter("# 没有 frontmatter\n"), None);
        assert!(frontmatter("\u{feff}---\nname: z\n---\n").is_some(), "BOM 开头也认");
    }

    /// 按目录名排序、没描述的跳过、没名字用目录名、路径是模型视角的。
    #[test]
    fn scan_is_sorted_and_skips_what_it_cannot_describe() {
        let root = tmp();
        put(&root, "b-web", "---\nname: web\ndescription: 做网页\n---\n");
        put(&root, "a-pdf", "---\ndescription: 处理 PDF\n---\n");
        put(&root, "c-empty", "---\nname: empty\n---\n");
        put(&root, ".hidden", "---\nname: h\ndescription: 藏着\n---\n");
        std::fs::create_dir_all(root.join("d-nofile")).unwrap();

        let got = scan(&root, "/workspace/skills/");
        assert_eq!(
            got,
            vec![
                Skill {
                    name: "a-pdf".into(),
                    description: "处理 PDF".into(),
                    path: "/workspace/skills/a-pdf/SKILL.md".into(),
                },
                Skill {
                    name: "web".into(),
                    description: "做网页".into(),
                    path: "/workspace/skills/b-web/SKILL.md".into(),
                },
            ]
        );
        assert!(scan(&root.join("nope"), "/x").is_empty(), "目录不存在就是没有");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn index_is_empty_without_skills_and_lists_them_otherwise() {
        assert_eq!(index(&[]), "");
        let s = index(&[Skill {
            name: "pdf".into(),
            description: "处理 PDF".into(),
            path: "/workspace/skills/pdf/SKILL.md".into(),
        }]);
        assert!(s.starts_with("- 技能（skill）"), "{s}");
        assert!(s.contains("\n  - pdf：处理 PDF（/workspace/skills/pdf/SKILL.md）"), "{s}");
    }

    /// 内置的 create-skill 自己就是一个合格的 skill：扫得出来、描述说清什么时候用、
    /// 正文里的目录是这台机器的。
    #[test]
    fn create_skill_is_a_valid_skill_about_this_machine() {
        let root = tmp();
        put(&root, CREATE_SKILL, &create_skill_md("/workspace/skills/"));
        let got = scan(&root, "/workspace/skills");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "create-skill");
        assert!(got[0].description.contains("安装"), "{}", got[0].description);
        let body = std::fs::read_to_string(root.join(CREATE_SKILL).join("SKILL.md")).unwrap();
        assert!(body.contains("git clone <地址> /workspace/skills/<名字>"), "{body}");
        assert!(!body.contains("//"), "目录拼接别出双斜杠：{body}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn long_descriptions_are_clipped() {
        let long = "字".repeat(MAX_DESCRIPTION + 10);
        let c = clip(&long, MAX_DESCRIPTION);
        assert_eq!(c.chars().count(), MAX_DESCRIPTION + 1);
        assert!(c.ends_with('…'));
        assert_eq!(clip("a\n  b", 10), "a b", "换行压成一个空格");
    }
}
