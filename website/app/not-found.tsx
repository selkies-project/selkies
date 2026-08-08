import Link from 'next/link';

export default function NotFound() {
  return (
    <main className="flex flex-1 flex-col items-center justify-center gap-4 p-8 text-center">
      <h1 className="text-2xl font-semibold">Page not found</h1>
      <p className="text-fd-muted-foreground">
        This page has moved or never existed.
      </p>
      <Link href="/" className="text-fd-primary font-medium underline">
        Back to the documentation
      </Link>
    </main>
  );
}
