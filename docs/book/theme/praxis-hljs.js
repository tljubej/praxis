// Syntax highlighting for Praxis code blocks.
//
// mdBook bundles highlight.js but knows nothing about Praxis, so a
// ```praxis fence renders unstyled. This registers the language and
// re-highlights the blocks that were skipped. If highlight.js is absent or its
// API changes, every `praxis` block simply stays plain text — the page is never
// broken by this file.
(function () {
    if (typeof hljs === "undefined" || typeof hljs.registerLanguage !== "function") {
        return;
    }

    hljs.registerLanguage("praxis", function (hljs) {
        var KEYWORDS = {
            keyword:
                "var fn struct enum match if else for while loop in break continue return read",
            literal: "true false None Some",
            built_in:
                "Int Float Bool Text Char Unit Option Vec Deque Map Set Counter MinHeap MaxHeap Grid BitSet Range " +
                "out dbg panic assert abs sign min max clamp gcd lcm " +
                "bfs bfs_distance dfs dijkstra a_star flood_fill",
        };

        return {
            name: "Praxis",
            aliases: ["px"],
            keywords: KEYWORDS,
            contains: [
                hljs.C_LINE_COMMENT_MODE,
                hljs.QUOTE_STRING_MODE,
                // A backtick template is an input-parser expression, not a
                // string: its `{name:parser}` captures are the interesting part.
                {
                    className: "string",
                    begin: "`",
                    end: "`",
                    contains: [
                        {
                            className: "subst",
                            begin: "\\{",
                            end: "\\}",
                        },
                    ],
                },
                hljs.C_NUMBER_MODE,
                {
                    className: "title.function",
                    beginKeywords: "fn",
                    end: /[({]/,
                    excludeEnd: true,
                    contains: [hljs.UNDERSCORE_TITLE_MODE],
                },
                {
                    className: "title.class",
                    beginKeywords: "struct enum",
                    end: /[{\n]/,
                    excludeEnd: true,
                    contains: [hljs.UNDERSCORE_TITLE_MODE],
                },
            ],
        };
    });

    var blocks = document.querySelectorAll("code.language-praxis, code.language-px");
    for (var i = 0; i < blocks.length; i++) {
        var block = blocks[i];
        if (block.dataset.praxisHighlighted) {
            continue;
        }
        block.dataset.praxisHighlighted = "1";
        try {
            hljs.highlightElement(block);
        } catch (_) {
            /* leave the block as plain text */
        }
    }
})();
