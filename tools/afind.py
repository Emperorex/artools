#!/usr/bin/env python3

import os
import fnmatch
import argparse
from queue import Queue, Empty
from threading import Thread
from typing import Iterator, Optional


# Default directories to ignore (similar to common dev tools)
DEFAULT_IGNORES = {".git", "node_modules", "__pycache__"}


def worker(task_queue: Queue, output_queue: Queue, pattern: str,
           ignore_dirs: set, max_depth: Optional[int]):
    """
    Worker thread:
    - Takes directories from task_queue
    - Scans them using os.scandir (fast)
    - Pushes matching paths to output_queue
    - Pushes subdirectories back to task_queue
    """
    while True:
        item = task_queue.get()

        if item is None:
            # Shutdown signal
            task_queue.task_done()
            break

        path, depth = item

        try:
            with os.scandir(path) as it:
                for entry in it:
                    # Skip ignored directories early
                    if entry.is_dir(follow_symlinks=False):
                        if entry.name in ignore_dirs:
                            continue

                        # Respect max depth
                        if max_depth is None or depth < max_depth:
                            task_queue.put((entry.path, depth + 1))

                    # Match file/directory name against pattern
                    if fnmatch.fnmatch(entry.name, pattern):
                        output_queue.put(entry.path)

        except PermissionError:
            # Skip directories we cannot access
            pass

        finally:
            task_queue.task_done()


def parallel_find(root: str,
                  pattern: str,
                  workers: int,
                  ignore_dirs: set,
                  max_depth: Optional[int]) -> Iterator[str]:
    """
    Main parallel find generator:
    - Starts worker threads
    - Feeds root directory
    - Yields results as they appear (streaming)
    """

    task_queue = Queue()
    output_queue = Queue()

    # Start worker threads
    threads = []
    for _ in range(workers):
        t = Thread(
            target=worker,
            args=(task_queue, output_queue, pattern, ignore_dirs, max_depth),
            daemon=True
        )
        t.start()
        threads.append(t)

    # Seed initial task
    task_queue.put((root, 0))

    def result_generator():
        """
        Yield results until all work is done.
        """
        while True:
            try:
                yield output_queue.get(timeout=0.1)
            except Empty:
                # If no tasks left and queue empty → done
                if task_queue.unfinished_tasks == 0:
                    break

    # Wait for completion
    task_queue.join()

    # Stop workers
    for _ in threads:
        task_queue.put(None)
    for t in threads:
        t.join()

    return result_generator()


def parse_args():
    """
    CLI argument parsing.
    """
    parser = argparse.ArgumentParser(
        description="Fast parallel find (Python version)"
    )

    parser.add_argument(
        "path",
        nargs="?",
        default=".",
        help="Root directory (default: current directory)"
    )

    parser.add_argument(
        "-n", "--name",
        default="*",
        help="Filename pattern (e.g. '*.py')"
    )

    parser.add_argument(
        "-j", "--jobs",
        type=int,
        default=4,
        help="Number of worker threads (default: 8)"
    )

    parser.add_argument(
        "--max-depth",
        type=int,
        default=None,
        help="Maximum recursion depth"
    )

    parser.add_argument(
        "--ignore",
        action="append",
        default=[],
        help="Additional directories to ignore"
    )

    return parser.parse_args()


def main():
    args = parse_args()

    ignore_dirs = DEFAULT_IGNORES.union(set(args.ignore))

    for path in parallel_find(
            root=args.path,
            pattern=args.name,
            workers=args.jobs,
            ignore_dirs=ignore_dirs,
            max_depth=args.max_depth
    ):
        print(path)


if __name__ == "__main__":
    main()
