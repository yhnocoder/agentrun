function languageOf(code) {
  const match = /(?:^|\s)language-([\w-]+)/.exec(code.className);
  return match ? match[1].replace(/^diff-/, "") : "";
}

function wrapButton(pre) {
  const button = document.createElement("button");
  button.type = "button";
  button.className = "code-wrap";
  button.textContent = "wrap";
  button.setAttribute("aria-pressed", "false");
  button.addEventListener("click", () => {
    const wrapped = pre.classList.toggle("wrapped");
    button.setAttribute("aria-pressed", String(wrapped));
  });
  return button;
}

document.addEventListener("DOMContentLoaded", () => {
  for (const pre of document.querySelectorAll("pre")) {
    const box = document.createElement("div");
    box.className = "code-scroll";
    const view = document.createElement("div");
    view.className = "code-view";
    pre.replaceWith(box);
    box.append(view);
    view.append(pre);
    const code = pre.querySelector("code");
    const bar = document.createElement("div");
    bar.className = "code-bar";
    const label = document.createElement("span");
    label.className = "code-lang";
    label.textContent = code ? languageOf(code) : "";
    bar.append(label);
    if (!pre.hasAttribute("data-line")) bar.append(wrapButton(pre));
    box.append(bar);
  }
});
