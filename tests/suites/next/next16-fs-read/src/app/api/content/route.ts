import { readFile } from "node:fs/promises";
import path from "node:path";
import { connection } from "next/server";

export async function GET() {
  await connection();
  const file = path.join(process.cwd(), "content", "hello.md");
  return new Response(await readFile(file, "utf8"));
}
