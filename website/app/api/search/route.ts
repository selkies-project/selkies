import { createFromSource } from 'fumadocs-core/search/server';
import { source } from '@/lib/source';

export const revalidate = false;

// staticGET emits the index as a file the browser fetches, which is the only
// form of search a static export can serve.
export const { staticGET: GET } = createFromSource(source);
