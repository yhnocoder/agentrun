window.MathJax = {
  tex: {
    inlineMath: [["$", "$"], ["\\(", "\\)"]],
    displayMath: [["$$", "$$"], ["\\[", "\\]"]],
    tags: "ams",
    packages: { "[-]": ["noundefined"] }
  },
  options: {
    ignoreHtmlClass: "term|mathjax_ignore",
    skipHtmlTags: ["script", "noscript", "style", "textarea", "pre", "code"]
  }
};
