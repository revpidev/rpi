// Model-catalog overlay: `/api/models/providers/{id}.json`
//
// Static fallback for these routes would SPA-fallback to index.html (200 +
// HTML), breaking the documented contract in remote_catalog_provider.rs:
// "未收录 provider 返回 404 = overlay 不可用". This function proxies the
// matching static file when it exists (preserving ETag / Last-Modified /
// cache-control from the asset), and returns a bare 404 otherwise.
export async function onRequestGet({ env, params, request }) {
  const id = params.id ?? params["id.json"];
  if (!id) {
    return new Response(null, { status: 404 });
  }
  const assetUrl = new URL(`/api/models/providers/${id}.json`, request.url);
  const asset = await env.ASSETS.fetch(assetUrl);
  // ASSETS.fetch 对未命中路径会走 SPA fallback（返回 200 + index.html），
  // 因此以 content-type 区分：不是 JSON 一律视为 catalog 未收录。
  const contentType = asset.headers.get("content-type") || "";
  if (asset.status !== 200 || contentType.includes("text/html")) {
    return new Response(null, { status: 404 });
  }
  return asset;
}
