import { useEffect, useState } from 'react'
import type { ContainerExtensions, DshContainer } from '../../shared/types/domain'

export type ContainerDetailsText = {
  containerDetails: string; back: string; activeProfile: string; profiles: string; addProfile: string; profilePlaceholder: string; plugins: string; skills: string; noPlugins: string; noSkills: string; containerSkill: string; diagnostics: string; version: string; path: string
}

type Props = {
  container: DshContainer
  details: ContainerExtensions | null
  text: ContainerDetailsText
  onBack: () => void
  onAddProfile: (profile: string) => Promise<void>
  onSelectProfile: (profile: string) => Promise<void>
}

export function ContainerDetails({ container, details, text, onBack, onAddProfile, onSelectProfile }: Props) {
  const [profile, setProfile] = useState(container.profile)
  const [newProfile, setNewProfile] = useState('')
  const [saving, setSaving] = useState(false)
  useEffect(() => { setProfile(container.profile) }, [container.id, container.profile])
  const selected = details?.profiles.find((item) => item.name === profile) ?? details?.profiles[0]
  async function selectProfile(value: string): Promise<void> { setSaving(true); try { await onSelectProfile(value); setProfile(value) } finally { setSaving(false) } }
  async function addProfile(): Promise<void> { const value = newProfile.trim(); if (!value) return; setSaving(true); try { await onAddProfile(value); setProfile(value); setNewProfile('') } finally { setSaving(false) } }
  return <section className="workspace container-detail-view">
    <button type="button" className="back-button" onClick={onBack}>← {text.back}</button>
    <p className="eyebrow">CONTAINER</p><h1>{container.name}</h1>
    <section className="container-detail-summary"><span>{container.id}</span><span>{text.version}: {container.version}</span></section>
    <section className="extensions-section"><div className="extensions-heading"><h2>{text.plugins}</h2><label>{text.activeProfile}<select value={selected?.name ?? ''} disabled={saving || !selected} onChange={(event) => { void selectProfile(event.target.value) }}>{details?.profiles.map((item) => <option key={item.name} value={item.name}>{item.name}</option>)}</select></label></div>
      <div className="profile-create"><input value={newProfile} placeholder={text.profilePlaceholder} onChange={(event) => { setNewProfile(event.target.value) }} /><button type="button" className="secondary" disabled={saving || !newProfile.trim()} onClick={() => { void addProfile() }}>{text.addProfile}</button></div>
      <div className="extension-list">{selected?.plugins.length ? selected.plugins.map((plugin) => <article key={plugin.name} className="extension-row"><div><strong>{plugin.name}</strong><p>{plugin.description ?? plugin.diagnostic ?? ''}</p></div><div><code>{plugin.version ?? '—'}</code>{plugin.path && <p className="path">{plugin.path}</p>}</div></article>) : <p className="empty-extension">{text.noPlugins}</p>}</div>
    </section>
    <section className="extensions-section"><h2>{text.skills}</h2><div className="extension-list">{details?.skills.length ? details.skills.map((skill) => <article key={skill.path} className="extension-row"><div><strong>{skill.name}</strong><p>{skill.description ?? skill.diagnostic ?? ''}</p></div><div><span className="extension-source">{text.containerSkill}</span><p className="path">{skill.path}</p></div></article>) : <p className="empty-extension">{text.noSkills}</p>}</div></section>
    {details?.diagnostics.length ? <section className="extension-diagnostics"><strong>{text.diagnostics}</strong>{details.diagnostics.map((item) => <p key={item}>{item}</p>)}</section> : null}
  </section>
}
