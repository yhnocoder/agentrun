const SKIPPED = ".nav, .page-head button, .code-bar, script, style, svg, mjx-container";

function fence(text, lang) {
  const longest = Math.max(2, ...(text.match(/`+/g) || []).map((run) => run.length));
  const marks = "`".repeat(longest + 1);
  return `${marks}${lang}\n${text.replace(/\n+$/, "")}\n${marks}`;
}

function inlineCode(text) {
  const marks = text.includes("`") ? "``" : "`";
  const pad = text.startsWith("`") || text.endsWith("`") ? " " : "";
  return `${marks}${pad}${text}${pad}${marks}`;
}

function inline(node) {
  if (node.nodeType === Node.TEXT_NODE) return node.textContent.replace(/\s+/g, " ");
  if (node.nodeType !== Node.ELEMENT_NODE || node.matches(SKIPPED)) return "";
  const inner = () => Array.from(node.childNodes, inline).join("");
  switch (node.tagName) {
    case "CODE":
      return inlineCode(node.textContent);
    case "A":
      return `[${inner().trim()}](${node.href})`;
    case "STRONG":
    case "B":
      return `**${inner().trim()}**`;
    case "EM":
    case "I":
      return `*${inner().trim()}*`;
    case "BR":
      return "\n";
    default:
      return inner();
  }
}

function cell(node) {
  return inline(node).trim().replace(/\|/g, "\\|");
}

function table(node) {
  const rows = Array.from(node.querySelectorAll("tr"), (row) => Array.from(row.cells, cell));
  const width = Math.max(...rows.map((row) => row.length));
  const line = (row) => `| ${Array.from({ length: width }, (_, i) => row[i] ?? "").join(" | ")} |`;
  const [head, ...body] = rows;
  return [line(head), line(Array(width).fill("---")), ...body.map(line)].join("\n");
}

function list(node, depth) {
  const ordered = node.tagName === "OL";
  return Array.from(node.children, (item, index) => {
    const nested = Array.from(item.children).filter((child) => child.matches("ul, ol"));
    const text = Array.from(item.childNodes)
      .filter((child) => !nested.includes(child))
      .map(inline)
      .join("")
      .trim();
    const marker = ordered ? `${index + 1}.` : "-";
    const lines = [`${"  ".repeat(depth)}${marker} ${text}`];
    for (const child of nested) lines.push(list(child, depth + 1));
    return lines.join("\n");
  }).join("\n");
}

function block(node) {
  if (node.nodeType !== Node.ELEMENT_NODE || node.matches(SKIPPED)) return [];
  if (node.matches(".term")) return [fence(node.textContent, "console")];
  switch (node.tagName) {
    case "H1":
    case "H2":
    case "H3":
    case "H4":
      return [`${"#".repeat(Number(node.tagName[1]))} ${inline(node).trim()}`];
    case "P":
    case "FIGCAPTION": {
      const text = inline(node).trim();
      return text ? [text] : [];
    }
    case "UL":
    case "OL":
      return [list(node, 0)];
    case "TABLE":
      return [table(node)];
    case "PRE": {
      const code = node.querySelector("code");
      const lang = code ? (/language-([\w-]+)/.exec(code.className) || [])[1] || "" : "";
      return [fence(node.textContent, lang.replace(/^diff-/, ""))];
    }
    case "BLOCKQUOTE":
      return [Array.from(node.children, block).flat().join("\n\n").replace(/^/gm, "> ")];
    case "HR":
      return ["---"];
    default:
      return Array.from(node.children, block).flat();
  }
}

function pageMarkdown() {
  const main = document.querySelector("main");
  return `${block(main).join("\n\n")}\n\nSource: ${location.href.split("#")[0]}\n`;
}

async function copyText(text) {
  try {
    await navigator.clipboard.writeText(text);
  } catch {
    const area = document.createElement("textarea");
    area.value = text;
    area.style.position = "fixed";
    area.style.opacity = "0";
    document.body.append(area);
    area.select();
    document.execCommand("copy");
    area.remove();
  }
}

document.addEventListener("DOMContentLoaded", () => {
  for (const button of document.querySelectorAll(".copy-page")) {
    const label = button.querySelector("span");
    const idle = label.textContent;
    button.addEventListener("click", async () => {
      await copyText(pageMarkdown());
      label.textContent = "Copied";
      button.classList.add("done");
      setTimeout(() => {
        label.textContent = idle;
        button.classList.remove("done");
      }, 2000);
    });
  }
});
