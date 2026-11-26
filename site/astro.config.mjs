// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

// Served by GitHub Pages at https://fz-maximize.github.io/fz-maximize/
// (.github/workflows/pages.yml). Internal links in content must include the base:
// /fz-maximize/getting-started/
export default defineConfig({
	site: 'https://fz-maximize.github.io',
	base: '/fz-maximize',
	trailingSlash: 'always',
	integrations: [
		starlight({
			title: 'fz-maximize',
			description:
				'Make Windows maximize fill the PowerToys FancyZones zone a window is in, instead of the whole monitor.',
			logo: { src: './public/favicon.svg', alt: '' },
			favicon: '/favicon.svg',
			social: [
				{ icon: 'github', label: 'GitHub', href: 'https://github.com/fz-maximize/fz-maximize' },
			],
			editLink: { baseUrl: 'https://github.com/fz-maximize/fz-maximize/edit/main/site/' },
			customCss: ['./src/styles/custom.css'],
			sidebar: [
				{
					label: 'Start here',
					items: [
						{ label: 'Getting started', slug: 'getting-started' },
						{ label: 'Using fz-maximize', slug: 'using' },
					],
				},
				{
					label: 'Reference',
					items: [
						{ label: 'Settings', slug: 'settings' },
						{ label: 'Command line', slug: 'command-line' },
					],
				},
				{
					label: 'Help',
					items: [
						{ label: 'Troubleshooting', slug: 'troubleshooting' },
						{ label: 'FAQ', slug: 'faq' },
					],
				},
				{
					label: 'About',
					items: [
						{ label: 'Privacy', slug: 'privacy' },
						{ label: 'Code signing policy', slug: 'code-signing' },
						{ label: 'Release notes', link: 'https://github.com/fz-maximize/fz-maximize/releases' },
						{ label: 'Contributing', link: 'https://github.com/fz-maximize/fz-maximize/blob/main/CONTRIBUTING.md' },
					],
				},
			],
		}),
	],
});
