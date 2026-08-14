import type { Language } from './shared/types/domain'

const COPY = {
  en: {
    versions: 'DSH Version Code', container: 'DSH Container', settings: 'Settings',
    chooseTitle: 'Choose DSH runtime directory', welcome: 'Set up DSH Box', welcomeNote: 'Choose a local folder for DSH Box data.',
    chooseDirectory: 'Choose folder', runtimeDirectory: 'Runtime directory', changeDirectory: 'Change directory',
    toolchainTitle: 'Bundled runtime', toolchainNote: 'DSH Box includes a private Node, npm, and pnpm runtime.', managed: 'Included with DSH Box', notFound: 'Unavailable', refresh: 'Refresh',
    versionTitle: 'DSH Version Code', versionNote: 'Versions are loaded from deepseek-ai/deepseek-harness.', noVersion: 'No DSH version installed', addVersion: 'Add version', install: 'Install', installed: 'Installed', uninstall: 'Uninstall', loadVersions: 'Load versions', installing: 'Installing…',
    containerTitle: 'DSH Container', notConfigured: 'Not configured', language: 'Language', storage: 'Local storage', toolchainSettings: 'Runtime', general: 'General', saved: 'Saved', service: 'Background service', restartService: 'Restart service', serviceRunning: 'Running', serviceStopped: 'Not running',
    githubMirror: 'GitHub mirror', githubMirrorNote: 'Prefix for GitHub URLs (version list, clones). Leave empty to connect directly.', npmRegistry: 'npm registry', npmRegistryNote: 'Registry used by pnpm installs inside DSH.', saveMirror: 'Save network settings',
    installedVersions: 'DSH version', containerName: 'Container name', namePlaceholder: 'My DSH workspace', containerProfile: 'Profile', profilePlaceholder: 'web', createContainer: 'Create container', creating: 'Creating…', noInstalledVersion: 'Install a DSH version first.',
    containers: 'Containers', start: 'Start', stop: 'Stop', open: 'Open', moreActions: 'More actions', rebuild: 'Rebuild', remove: 'Delete', running: 'Running', stopped: 'Stopped',
    containerDetails: 'Container details', back: 'Back', activeProfile: 'Active profile', profiles: 'Profiles', addProfile: 'Add profile', plugins: 'Plugins', skills: 'Skills', logs: 'Logs', hostLog: 'Host', rebuildLog: 'Rebuild', webviewLog: 'WebView', logRefresh: 'Refresh', workspace: 'Workspace', noPlugins: 'No enabled plugins in this profile.', noSkills: 'No container skills.', containerSkill: 'Container Skill', diagnostics: 'Diagnostics', version: 'DSH version', path: 'Path', addExtension: 'Add', upgrade: 'Upgrade', scanWorkspace: 'Scan workspace', importWorkspace: 'Import to repo', extensionSource: 'GitHub URL or local tarball path', browseArchive: 'Choose tarball', adding: 'Queued…', openInBrowser: 'Open in browser', singleAdd: 'Single', bundleAdd: 'Bundle', conflictOverwrite: 'Overwrite', conflictKeep: 'Keep',
    pluginRepo: 'Plugin Repository', pluginRepoNote: 'Import plugins, export a tarball, or make them available to your containers.', noRepositoryPlugins: 'No imported plugins yet.', exportPlugin: 'Export tarball', installTo: 'Install to', profile: 'Profile', exporting: 'Exporting…', pluginsTab: 'Plugins', bundles: 'Bundles', createBundle: 'Create bundle', bundleName: 'Bundle name', selectEntries: 'Select extensions', quickExport: 'Quick export', fullExport: 'Full export', noBundles: 'No bundles yet.', githubOnly: 'URL only', importBundle: 'Import bundle', bundleRefNote: 'is referenced by bundles', bundleRefDelete: 'Deleting it also removes it from those bundles. Continue?',
    tasks: 'Tasks', taskRunning: 'running', recentTasks: 'Recent', cancel: 'Cancel', retry: 'Retry', viewLog: 'View log', close: 'Close',
  },
  'zh-CN': {
    versions: 'DSH 版本代码', container: 'DSH 容器', settings: '设置',
    chooseTitle: '选择 DSH 运行目录', welcome: '设置 DSH Box', welcomeNote: '选择一个本地文件夹来存储 DSH Box 数据。',
    chooseDirectory: '选择文件夹', runtimeDirectory: '运行目录', changeDirectory: '更改目录',
    toolchainTitle: '内置运行时', toolchainNote: 'DSH Box 已内置私有的 Node、npm 与 pnpm 运行时。', managed: '随 DSH Box 提供', notFound: '不可用', refresh: '刷新',
    versionTitle: 'DSH 版本代码', versionNote: '版本直接从 deepseek-ai/deepseek-harness 获取。', noVersion: '尚未安装 DSH 版本', addVersion: '添加版本', install: '安装', installed: '已安装', uninstall: '卸载', loadVersions: '获取版本', installing: '正在安装…',
    containerTitle: 'DSH 容器', notConfigured: '尚未配置', language: '语言', storage: '本地存储', toolchainSettings: '运行时', general: '通用', saved: '已保存', service: '后台服务', restartService: '重启服务', serviceRunning: '运行中', serviceStopped: '未运行',
    githubMirror: 'GitHub 镜像', githubMirrorNote: 'GitHub 链接前缀（版本列表、clone）。留空则直连。', npmRegistry: 'npm 仓库镜像', npmRegistryNote: 'DSH 内 pnpm install 使用的仓库。', saveMirror: '保存网络设置',
    installedVersions: 'DSH 版本', containerName: '容器名称', namePlaceholder: '我的 DSH 工作区', containerProfile: 'Profile', profilePlaceholder: 'web', createContainer: '创建容器', creating: '正在创建…', noInstalledVersion: '请先安装一个 DSH 版本。',
    containers: '容器列表', start: '启动', stop: '停止', open: '进入使用', moreActions: '更多操作', rebuild: '重新构建', remove: '删除', running: '运行中', stopped: '已停止',
    containerDetails: 'Container 详情', back: '返回', activeProfile: '当前 Profile', profiles: 'Profiles', addProfile: '新增 Profile', plugins: '插件', skills: '技能', logs: '日志', hostLog: 'Host', rebuildLog: '构建', webviewLog: 'WebView', logRefresh: '刷新', workspace: '工作区', noPlugins: '这个 Profile 没有启用插件。', noSkills: '没有 Container 专属 Skill。', containerSkill: 'Container Skill', diagnostics: '诊断信息', version: 'DSH 版本', path: 'Path', addExtension: '添加', upgrade: '升级', scanWorkspace: '扫描工作区', importWorkspace: '导入仓库', extensionSource: 'GitHub URL 或本地 tarball 路径', browseArchive: '选择 tarball', adding: '已加入任务…', openInBrowser: '浏览器打开', singleAdd: '单个', bundleAdd: '整合包', conflictOverwrite: '覆盖', conflictKeep: '保留',
    pluginRepo: '插件仓库', pluginRepoNote: '在这里导入插件、导出 tarball，或供 Container 选择使用。', noRepositoryPlugins: '暂时没有已导入的插件。', exportPlugin: '导出 tarball', installTo: '安装到', profile: 'Profile', exporting: '正在导出…', pluginsTab: '插件', bundles: '整合包', createBundle: '创建整合包', bundleName: '整合包名称', selectEntries: '选择扩展条目', quickExport: '快速导出', fullExport: '全量导出', noBundles: '暂无整合包。', githubOnly: '仅 URL', importBundle: '导入整合包', bundleRefNote: '被整合包引用', bundleRefDelete: '删除后整合包中也会移除。继续？',
    tasks: '任务', taskRunning: '进行中', recentTasks: '最近任务', cancel: '取消', retry: '重试', viewLog: '查看日志', close: '关闭',
  },
} as const

export type Text = (typeof COPY)[Language]

export function pickText(language: Language): Text {
  return COPY[language]
}
