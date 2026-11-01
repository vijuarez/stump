import { MetadataProvider } from '@stump/graphql'

export const PROVIDER_LABELS: Record<MetadataProvider, string> = {
	[MetadataProvider.Hardcover]: 'Hardcover',
	[MetadataProvider.ComicVine]: 'Comic Vine',
	[MetadataProvider.Anilist]: 'AniList',
}

export const PROVIDERS = Object.values(MetadataProvider)

/** Providers whose public API works without credentials, so no token is collected */
const PROVIDERS_WITHOUT_API_KEY: MetadataProvider[] = [MetadataProvider.Anilist]

export const providerRequiresApiKey = (provider?: MetadataProvider | null) =>
	!!provider && !PROVIDERS_WITHOUT_API_KEY.includes(provider)
