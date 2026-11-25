"""Text helpers."""


def slugify(text, max_length=None):
    """Turn ``text`` into a URL slug.

    - Accented letters become their plain ASCII letters ("Crème" gives
      "creme"); every other character outside ASCII is dropped.
    - Letters are lowercased.
    - Every run of characters that are not ASCII letters or digits becomes a
      single "-".
    - The slug never starts or ends with "-".
    - With ``max_length``, a slug longer than that keeps as many whole words
      (the parts between "-") from the start as fit in ``max_length``
      characters, joined by "-" as before, so it never ends with "-". When
      even the first word is longer than ``max_length``, it is cut to
      ``max_length`` characters.
    - Text with nothing left gives "".

    >>> slugify("Hello, World!")
    'hello-world'
    >>> slugify("The quick brown fox", max_length=13)
    'the-quick'
    """
    raise NotImplementedError
