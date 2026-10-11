<script>
	import DocHeader from '$lib/components/DocHeader.svelte';
	import SiteFooter from '$lib/components/SiteFooter.svelte';
	import { renderReleaseMarkdown } from '$lib/releaseMarkdown.js';

	/** @type {{ posts: { slug: string, title: string, date: string, description: string, body: string }[], selected?: { slug: string, title: string, date: string, description: string, body: string } | null }} */
	let { posts, selected = null } = $props();
	const visible = $derived(selected ? [selected] : posts);
</script>

<svelte:head>
	<title>{selected ? `${selected.title} · madmail` : 'Blog · madmail'}</title>
	<meta
		name="description"
		content={selected?.description || 'Notes from the Madmail project.'}
	/>
	<link rel="alternate" type="application/rss+xml" title="Madmail blog" href="/blog/rss.xml" />
</svelte:head>

<DocHeader section="Blog" />

<div class="blog-page">
	<main>
		<div class="page-heading">
			{#if selected}
				<p class="back"><a href="/blog">Blog</a></p>
				<h1>{selected.title}</h1>
				<p><time datetime={selected.date}>{selected.date}</time></p>
			{:else}
				<h1>Blog</h1>
				<p>Notes from the Madmail project. <a href="/blog/rss.xml">RSS</a></p>
			{/if}
		</div>
		{#if visible.length === 0}
			<p>No posts yet.</p>
		{:else}
			{#each visible as post (post.slug)}
				<article>
					{#if !selected}
						<h2><a href="/blog/{post.slug}">{post.title}</a></h2>
						<p class="date"><time datetime={post.date}>{post.date}</time></p>
						{#if post.description}<p>{post.description}</p>{/if}
					{:else}
						<div class="notes">{@html renderReleaseMarkdown(post.body)}</div>
					{/if}
				</article>
			{/each}
		{/if}
	</main>
	<SiteFooter />
</div>

<style>
	.blog-page {
		position: relative;
		z-index: 1;
		background: var(--color-bg);
		padding-bottom: 3rem;
	}
	main {
		max-width: 48rem;
		margin: 0 auto;
		padding: 6rem 2rem 0;
	}
	.page-heading {
		margin-bottom: 3rem;
	}
	h1 {
		font-size: clamp(2.5rem, 5vw, 3.5rem);
		letter-spacing: -0.04em;
		margin: 0.5rem 0 1rem;
	}
	.page-heading > p:last-child,
	.date,
	.back {
		color: var(--color-text-muted);
		line-height: 1.6;
	}
	.back {
		margin: 0;
	}
	a {
		color: var(--color-text);
	}
	article {
		padding: 0 0 2.5rem;
		margin-bottom: 2.5rem;
		border-bottom: 1px solid var(--color-border);
	}
	h2 {
		margin: 0 0 0.5rem;
	}
	h2 a {
		text-decoration: none;
	}
	h2 a:hover {
		text-decoration: underline;
	}
	.notes {
		overflow-wrap: anywhere;
		line-height: 1.75;
	}
	.notes :global(h2),
	.notes :global(h3) {
		margin: 2rem 0 0.75rem;
		line-height: 1.3;
	}
	.notes :global(p),
	.notes :global(ul),
	.notes :global(ol) {
		margin: 1rem 0;
	}
	.notes :global(pre) {
		overflow-x: auto;
		padding: 1rem;
		background: var(--color-surface-code);
		border-radius: 0.5rem;
	}
	.notes :global(code) {
		font-size: 0.85em;
		background: var(--color-surface-code);
		padding: 0.15em 0.3em;
		border-radius: 0.2rem;
	}
	.notes :global(pre code) {
		padding: 0;
	}
	.notes :global(blockquote) {
		border-left: 2px solid var(--color-border-strong);
		margin-left: 0;
		padding-left: 1rem;
		color: var(--color-text-muted);
	}
	.notes :global(img) {
		max-width: 100%;
		height: auto;
	}
	@media (max-width: 700px) {
		main {
			padding: 5rem 1rem 0;
		}
	}
</style>
