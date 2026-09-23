import { docsLlms } from '@/lib/llms';

export const revalidate = false;

export async function GET() {
  return new Response(await docsLlms.full(), { headers: { 'Content-Type': 'text/plain' } });
}
