// scripts/ 目录为仓库内部工具，不对外提供。
export async function onRequest() {
  return new Response(null, { status: 404 });
}
