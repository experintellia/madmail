import { error } from '@sveltejs/kit';
import { postBySlug, posts } from '$lib/blog.js';

export function entries() {
	return posts.map((post) => ({ slug: post.slug }));
}

/** @param {{ params: { slug: string } }} event */
export function load({ params }) {
	const post = postBySlug(params.slug);
	if (!post) error(404, 'Post not found');
	return { post };
}
