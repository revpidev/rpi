// 仓库内部文件不对外提供（direct upload 下 .assetsignore/_redirects-404 均不生效，
// 用路由级函数直接 404）。
export async function onRequest() {
  return new Response(null, { status: 404 });
}
