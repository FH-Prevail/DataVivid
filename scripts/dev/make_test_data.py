"""Generates test files for DataVivid in the given directory (default: ./testdata).

  sales_1m.csv   1,000,000 rows, UTF-8, comma-separated, ~8% missing discounts,
                 quoted fields with embedded quotes
  eu_prices.csv  200 rows, Windows-1252, semicolon-separated, decimal comma,
                 dd.mm.yyyy dates and umlauts

Run:  python scripts/dev/make_test_data.py [outdir]
Then e.g.  datavivid --convert testdata/sales_1m.csv testdata/sales_1m.parquet
"""
import datetime
import os
import random
import sys

out = sys.argv[1] if len(sys.argv) > 1 else "testdata"
os.makedirs(out, exist_ok=True)
random.seed(7)

regions = ["North", "South", "East", "West", "Central"]
products = ["Widget", "Gadget", "Doohickey", "Sprocket", "Gizmo", "Thingamajig"]
with open(os.path.join(out, "sales_1m.csv"), "w", encoding="utf-8", newline="") as f:
    f.write("order_id,date,region,product,quantity,unit_price,discount,customer,notes\n")
    d0 = datetime.date(2023, 1, 1)
    for i in range(1_000_000):
        d = d0 + datetime.timedelta(days=random.randint(0, 700))
        q = random.randint(1, 50)
        p = round(random.uniform(2, 250), 2)
        disc = "" if random.random() < 0.08 else round(random.choice([0, 0, 0, 0.05, 0.1, 0.15]), 2)
        cust = f"C{random.randint(1, 20000):05d}"
        note = random.choice(["", "", "", "rush", "gift wrap", "call before delivery", 'said "fragile"'])
        if '"' in note:
            note = '"' + note.replace('"', '""') + '"'
        reg = random.choice(regions) if random.random() > 0.02 else ""
        f.write(f"{100000 + i},{d.isoformat()},{reg},{random.choice(products)},{q},{p},{disc},{cust},{note}\n")

with open(os.path.join(out, "eu_prices.csv"), "w", encoding="cp1252", newline="") as f:
    f.write("Artikel;Größe;Preis €;Datum\n")
    for i in range(200):
        price = f"{random.uniform(1, 999):.2f}".replace(".", ",")
        date = (datetime.date(2024, 1, 1) + datetime.timedelta(days=i)).strftime("%d.%m.%Y")
        f.write(f"Möbel {i};{random.choice(['S', 'M', 'L', 'XL'])};{price};{date}\n")

print(f"wrote test data to {out}")
