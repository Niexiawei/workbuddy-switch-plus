export const GITHUB_OWNER = "Hyact";
export const GITHUB_REPO = "workbuddy-switch-plus";
export const GITHUB_REPOSITORY_URL = `https://github.com/${GITHUB_OWNER}/${GITHUB_REPO}`;
export const GITHUB_RELEASE_URL = `${GITHUB_REPOSITORY_URL}/releases/latest`;

/**
 * 二改基底：本项目的上游仓库。
 *
 * 只用于设置页的出处展示与「大概去哪儿看原始实现」的入口，不参与版本检查——
 * 两个仓库的版本号各走各的，混在一起会让用户以为上游发版就是他该升级。
 */
export const UPSTREAM_OWNER = "changexbc";
export const UPSTREAM_REPO = "workbuddy-switch";
export const UPSTREAM_REPOSITORY_URL = `https://github.com/${UPSTREAM_OWNER}/${UPSTREAM_REPO}`;

/** 用系统默认浏览器打开外链（桌面端唯一形态，走 Tauri opener）。 */
export async function openExternalUrl(url: string): Promise<void> {
  const { openUrl } = await import("@tauri-apps/plugin-opener");
  await openUrl(url);
}

export async function openReleaseUrl(url = GITHUB_RELEASE_URL): Promise<void> {
  await openExternalUrl(url);
}
