SELECT "SearchPhrase", MIN("URL"), MIN("Title"), COUNT(*) AS c, COUNT(DISTINCT "UserID") FROM hits WHERE "Title" LIKE '%Google%' AND "URL" NOT LIKE '%.google.%' AND CAST("SearchPhrase" AS TEXT) <> '' GROUP BY "SearchPhrase" ORDER BY c DESC LIMIT 10;
