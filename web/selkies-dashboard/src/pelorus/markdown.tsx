/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The Markdown the agent's answers are written in, drawn by the page and the
 * dashboard's panel alike: the few blocks and marks models reach for, with no
 * HTML of the model's own ever put in the page.
 * @module
 */
import * as React from "react";

/** Draws the inline marks of a line: `**bold**` and `` `code` ``. */
export function Inline({ text }: { text: string }) {
  const pieces = text.split(/(\*\*[^*]+\*\*|`[^`]+`)/g);
  return (
    <>
      {pieces.map((piece, index) => {
        if (piece.startsWith("**") && piece.endsWith("**") && piece.length > 4) return <strong key={index}>{piece.slice(2, -2)}</strong>;
        if (piece.startsWith("`") && piece.endsWith("`") && piece.length > 2) {
          return <code key={index} className="rounded bg-muted px-1 py-0.5 font-mono text-[0.9em]">{piece.slice(1, -1)}</code>;
        }
        return <React.Fragment key={index}>{piece}</React.Fragment>;
      })}
    </>
  );
}

/**
 * Draws an answer written in the Markdown models reach for: paragraphs,
 * headings, bulleted and numbered lists, fenced code, and the inline marks.
 * Anything else is shown as written.
 * @param props.text The answer.
 * @param props.className The type size and anything else the place draws it with.
 */
export function Markdown({ text, className = "text-sm" }: { text: string; className?: string }) {
  const blocks: React.ReactNode[] = [];
  const lines = text.split("\n");
  let index = 0;
  while (index < lines.length) {
    const line = lines[index];
    if (line.startsWith("```")) {
      const code: string[] = [];
      index += 1;
      while (index < lines.length && !lines[index].startsWith("```")) code.push(lines[index++]);
      index += 1;
      blocks.push(<pre key={blocks.length} className="overflow-x-auto rounded-md bg-muted p-2 font-mono text-xs">{code.join("\n")}</pre>);
      continue;
    }
    const heading = /^(#{1,4})\s+(.*)$/.exec(line);
    if (heading) {
      blocks.push(<div key={blocks.length} className="mt-1 font-semibold"><Inline text={heading[2]} /></div>);
      index += 1;
      continue;
    }
    const bullet = /^\s*([-*]|\d+[.)])\s+/;
    if (bullet.test(line)) {
      const items: string[] = [];
      const ordered = /^\s*\d/.test(line);
      while (index < lines.length && bullet.test(lines[index])) items.push(lines[index++].replace(bullet, ""));
      const List = ordered ? "ol" : "ul";
      blocks.push(
        <List key={blocks.length} className={`ml-5 ${ordered ? "list-decimal" : "list-disc"}`}>
          {items.map((item, at) => <li key={at}><Inline text={item} /></li>)}
        </List>,
      );
      continue;
    }
    if (line.trim() === "") {
      index += 1;
      continue;
    }
    const paragraph: string[] = [];
    while (index < lines.length && lines[index].trim() !== "" && !lines[index].startsWith("```") && !bullet.test(lines[index]) && !/^#{1,4}\s/.test(lines[index])) {
      paragraph.push(lines[index++]);
    }
    blocks.push(<p key={blocks.length} className="whitespace-pre-wrap"><Inline text={paragraph.join("\n")} /></p>);
  }
  return <div className={`flex flex-col gap-2 leading-relaxed ${className}`}>{blocks}</div>;
}
